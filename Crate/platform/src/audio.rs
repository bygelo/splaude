//! Microphone capture.
//!
//! `AVAudioEngine` installed a tap and handed back a converted buffer; cpal
//! hands back whatever the device natively speaks — any rate, any channel
//! count, any of a dozen sample formats. Everything downstream of the callback
//! is therefore a conversion problem, and it lives in [`crate::resample`] so it
//! can be tested without a device.
//!
//! # Why a dedicated thread
//!
//! cpal 0.18 does promise `Send + Sync` streams on every host it ships, but
//! that is a backend guarantee, not a language one — a `Stream` is a handle to
//! a callback the OS runs, and on several platforms historically it was
//! `!Send`. Rather than bet the app crate's threading model on that promise
//! holding, the stream never leaves the thread that built it: `start` spawns a
//! worker that opens the device, parks on a stop flag, and drops the stream on
//! its way out. What crosses a thread boundary is only atomics, so `Capture`
//! itself is unconditionally `Send` no matter what the backend does.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use splaude_core::diagnostic;
use splaude_core::speech::SpeechAudioFormat;

use crate::resample::{level_of, smooth, Resampler};

/// How long the worker sleeps between stop checks. Short enough that a key-up
/// releases the microphone before the user notices, long enough not to spin.
const STOP_POLL: Duration = Duration::from_millis(20);

/// Cap on backend start-up. WASAPI can sit for a long time on a device another
/// process is grabbing, and `start` is called from the hotkey path — an
/// unbounded wait there reads as a frozen app.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// Taps the default input device and hands back PCM matching `format`, plus a
/// smoothed 0…1 level for the meter.
pub struct Capture {
    stop: Arc<AtomicBool>,
    /// f32 bits. Written from the audio callback, so it cannot be a lock.
    peak: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn start(
        format: SpeechAudioFormat,
        on_audio: Box<dyn Fn(Vec<u8>) + Send + 'static>,
        on_level: Box<dyn Fn(f32) + Send + 'static>,
    ) -> Result<Self> {
        // The resampler mixes to mono unconditionally, so anything else would
        // be a silent lie about what the callback emits.
        if format.channel_count != 1 {
            return Err(anyhow!(
                "capture only produces mono, not {}ch",
                format.channel_count
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU32::new(0));
        let (ready, opened) = mpsc::channel::<Result<(), String>>();

        let worker = thread::Builder::new().name("splaude-audio".into()).spawn({
            let stop = Arc::clone(&stop);
            let peak = Arc::clone(&peak);
            move || match open(format, on_audio, on_level, &peak) {
                Ok(running) => {
                    let _ = ready.send(Ok(()));
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(STOP_POLL);
                    }
                    running.finish();
                }
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                }
            }
        })?;

        let mut capture = Self {
            stop,
            peak,
            worker: Some(worker),
        };

        // Failures are reported by the worker rather than raised here, so the
        // stream can be built on the thread that will own it.
        match opened.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(message)) => {
                capture.stop();
                Err(anyhow!(message))
            }
            Err(_) => {
                capture.stop();
                Err(anyhow!("microphone did not start within the timeout"))
            }
        }
    }

    pub fn stop(&mut self) {
        // `take` makes this idempotent: stop-then-drop must not join twice.
        let Some(worker) = self.worker.take() else {
            return;
        };
        self.stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
    }

    /// Loudest level seen during the last run, so the log can tell "mic sent
    /// nothing" from "mic sent silence".
    pub fn last_peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }
}

impl Drop for Capture {
    /// A dropped `Capture` that left its worker running would hold the
    /// microphone open for the life of the process.
    fn drop(&mut self) {
        self.stop();
    }
}

// MARK: - Worker

/// Live capture, owned by the worker thread and nothing else.
struct Running {
    /// Held only to keep the callback alive; dropping it stops capture.
    stream: cpal::Stream,
    byte_sent: Arc<AtomicUsize>,
    peak: Arc<AtomicU32>,
}

impl Running {
    fn finish(self) {
        // Drop first: the counters must not move under the log line.
        drop(self.stream);
        diagnostic::log(
            "audio",
            format!(
                "sent {} bytes, peak level {:.2}",
                self.byte_sent.load(Ordering::Relaxed),
                f32::from_bits(self.peak.load(Ordering::Relaxed)),
            ),
        );
        // The Swift build also pushed a zero level at this point. Here the
        // level callback belongs to the audio closure, which is already gone,
        // and the contract's `Box<dyn Fn>` is not `Sync` to share back out.
    }
}

fn open(
    format: SpeechAudioFormat,
    on_audio: Box<dyn Fn(Vec<u8>) + Send + 'static>,
    on_level: Box<dyn Fn(f32) + Send + 'static>,
    peak: &Arc<AtomicU32>,
) -> Result<Running> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no input device available"))?;

    let source = usable_config(&device, format.sample_rate)?;
    let source_rate = source.sample_rate();
    let channel_count = source.channels();

    diagnostic::log(
        "audio",
        format!(
            "capturing {source_rate} Hz x {channel_count}ch -> {} Hz mono int16",
            format.sample_rate
        ),
    );

    let byte_sent = Arc::new(AtomicUsize::new(0));
    let tap = Tap {
        resampler: Resampler::new(source_rate, channel_count, format.sample_rate),
        scratch: Vec::new(),
        level: 0.0,
        on_audio,
        on_level,
        peak: Arc::clone(peak),
        byte_sent: Arc::clone(&byte_sent),
    };

    let config = source.config();
    let stream = match source.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, tap),
        SampleFormat::I16 => build::<i16>(&device, config, tap),
        SampleFormat::U16 => build::<u16>(&device, config, tap),
        SampleFormat::I32 => build::<i32>(&device, config, tap),
        other => Err(anyhow!(
            "device does not support any usable config: sample format {other}"
        )),
    }?;

    // cpal streams are built paused; without this the callback never fires and
    // the take looks like a dead microphone.
    stream.play()?;

    Ok(Running {
        stream,
        byte_sent,
        peak: Arc::clone(peak),
    })
}

/// Everything the audio callback owns. Boxed up here so the sample-format
/// dispatch above stays one line per format.
struct Tap {
    resampler: Resampler,
    /// Reused across callbacks: allocating inside an audio callback is how you
    /// earn dropouts.
    scratch: Vec<f32>,
    level: f32,
    on_audio: Box<dyn Fn(Vec<u8>) + Send + 'static>,
    on_level: Box<dyn Fn(f32) + Send + 'static>,
    peak: Arc<AtomicU32>,
    byte_sent: Arc<AtomicUsize>,
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut tap: Tap) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        move |sample: &[T], _: &cpal::InputCallbackInfo| {
            tap.scratch.clear();
            tap.scratch
                .extend(sample.iter().map(|value| f32::from_sample(*value)));

            // Peak tracks the raw reading, not the smoothed one — smoothing
            // exists for the eye and would under-report a brief loud moment.
            let incoming = level_of(&tap.scratch);
            raise_peak(&tap.peak, incoming);
            tap.level = smooth(tap.level, incoming);
            (tap.on_level)(tap.level);

            let pcm = tap.resampler.push(&tap.scratch);
            if !pcm.is_empty() {
                tap.byte_sent.fetch_add(pcm.len(), Ordering::Relaxed);
                (tap.on_audio)(pcm);
            }
        },
        |error| diagnostic::log("audio", format!("stream error: {error}")),
        Some(START_TIMEOUT),
    )?;
    Ok(stream)
}

// MARK: - Configuration

/// Prefers the device's own default, which on a shared-mode backend is the only
/// config that avoids a resample in the driver as well as here.
fn usable_config(device: &cpal::Device, target_rate: u32) -> Result<SupportedStreamConfig> {
    if let Ok(config) = device.default_input_config() {
        if is_usable(config.sample_format()) {
            return Ok(config);
        }
    }

    let candidate = device
        .supported_input_configs()
        .map_err(|error| anyhow!("could not query input configs: {error}"))?;

    choose_config(candidate, target_rate)
        .ok_or_else(|| anyhow!("device does not support any usable config"))
}

/// Fewest channel first (less to mix down), then the format we lose least
/// converting from, then the rate closest to free.
fn choose_config(
    candidate: impl Iterator<Item = SupportedStreamConfigRange>,
    target_rate: u32,
) -> Option<SupportedStreamConfig> {
    candidate
        .filter(|range| is_usable(range.sample_format()) && range.channels() >= 1)
        .min_by_key(|range| (range.channels(), preference(range.sample_format())))
        .map(|range| {
            // Tapping at the target rate skips the FIR entirely.
            range
                .try_with_sample_rate(target_rate)
                .or_else(|| range.try_with_standard_sample_rate())
                .unwrap_or_else(|| range.with_max_sample_rate())
        })
}

/// Lower is better. [`u8::MAX`] means "cannot convert", which is also the
/// usability test.
fn preference(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I32 => 2,
        SampleFormat::U16 => 3,
        _ => u8::MAX,
    }
}

fn is_usable(format: SampleFormat) -> bool {
    preference(format) != u8::MAX
}

/// Raises the stored peak if `incoming` beats it. Compare-and-swap rather than
/// a mutex because this runs in the audio callback, where blocking drops audio.
fn raise_peak(peak: &AtomicU32, incoming: f32) {
    let mut current = peak.load(Ordering::Relaxed);
    while f32::from_bits(current) < incoming {
        match peak.compare_exchange_weak(
            current,
            incoming.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use cpal::SupportedBufferSize;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn the_target_rate_is_taken_when_the_device_offers_it() {
        let chosen = choose_config(
            [range(1, 8_000, 48_000, SampleFormat::F32)].into_iter(),
            16_000,
        )
        .expect("a usable config");
        assert_eq!(chosen.sample_rate(), 16_000);
    }

    #[test]
    fn a_device_that_cannot_reach_the_target_falls_back_to_a_standard_rate() {
        let chosen = choose_config(
            [range(1, 44_100, 48_000, SampleFormat::F32)].into_iter(),
            16_000,
        )
        .expect("a usable config");
        assert_eq!(chosen.sample_rate(), 48_000);
    }

    #[test]
    fn an_exotic_rate_range_still_yields_something_playable() {
        let chosen = choose_config(
            [range(1, 96_000, 192_000, SampleFormat::I16)].into_iter(),
            16_000,
        )
        .expect("a usable config");
        assert_eq!(chosen.sample_rate(), 192_000);
    }

    #[test]
    fn fewer_channel_wins_over_a_nicer_format() {
        let chosen = choose_config(
            [
                range(2, 16_000, 48_000, SampleFormat::F32),
                range(1, 16_000, 48_000, SampleFormat::I16),
            ]
            .into_iter(),
            16_000,
        )
        .expect("a usable config");
        assert_eq!(chosen.channels(), 1);
        assert_eq!(chosen.sample_format(), SampleFormat::I16);
    }

    #[test]
    fn f32_wins_at_equal_channel_count() {
        let chosen = choose_config(
            [
                range(2, 16_000, 48_000, SampleFormat::U16),
                range(2, 16_000, 48_000, SampleFormat::F32),
            ]
            .into_iter(),
            16_000,
        )
        .expect("a usable config");
        assert_eq!(chosen.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn a_device_offering_nothing_convertible_is_an_error_not_a_panic() {
        assert!(choose_config(
            [range(1, 16_000, 48_000, SampleFormat::I64)].into_iter(),
            16_000
        )
        .is_none());
        assert!(choose_config(std::iter::empty(), 16_000).is_none());
    }

    #[test]
    fn every_format_we_dispatch_on_is_reported_usable() {
        for format in [
            SampleFormat::F32,
            SampleFormat::I16,
            SampleFormat::U16,
            SampleFormat::I32,
        ] {
            assert!(is_usable(format), "{format} should be usable");
        }
        assert!(!is_usable(SampleFormat::I24));
    }

    #[test]
    fn peak_keeps_the_loudest_reading() {
        let peak = AtomicU32::new(0);
        raise_peak(&peak, 0.3);
        raise_peak(&peak, 0.9);
        raise_peak(&peak, 0.1);
        assert_eq!(f32::from_bits(peak.load(Ordering::Relaxed)), 0.9);
    }

    #[test]
    fn a_run_that_never_hears_anything_reads_zero() {
        let peak = AtomicU32::new(0);
        assert_eq!(f32::from_bits(peak.load(Ordering::Relaxed)), 0.0);
        // Silence still produces a level, just a tiny one — the distinction the
        // diagnostic log exists to make.
        raise_peak(&peak, level_of(&[0.0; 128]));
        assert!(f32::from_bits(peak.load(Ordering::Relaxed)) < 0.01);
    }

    #[test]
    fn a_non_mono_format_is_refused_rather_than_silently_mixed() {
        let format = SpeechAudioFormat {
            sample_rate: 16_000,
            channel_count: 2,
        };
        let started = Capture::start(format, Box::new(|_| {}), Box::new(|_| {}));
        assert!(started.is_err());
    }

    #[test]
    fn capture_stays_send_whatever_the_backend_does() {
        fn assert_send<T: Send>() {}
        assert_send::<Capture>();
    }
}
