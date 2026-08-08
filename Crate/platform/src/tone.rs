//! A short tone at each end of a take.
//!
//! The Swift build plays two system sounds — `Tink` on start, `Pop` on stop —
//! and there is no portable equivalent to reach for: nothing ships a named
//! sound on all three platforms, and a sound *file* would mean an audio decoder
//! and an asset in a repository whose only other asset is a macOS `.icns`.
//!
//! So the tone is synthesised. `cpal` is already here for the microphone and
//! opens an output stream with the same three calls, which keeps this at zero
//! new dependencies — including on Linux, where the build carries no tray and
//! must not grow anything.
//!
//! Everything here is best-effort by design. A machine with no output device,
//! or one whose default device is being held exclusively by something else, is
//! not a machine that should lose its dictation over a beep.

use std::f32::consts::TAU;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use splaude_core::diagnostic;

/// How long a tone sounds.
///
/// Short enough to be an acknowledgement rather than an event — the stop tone
/// plays while the user is already reading their words appear, and anything
/// longer would still be sounding when they start talking again.
const LENGTH: Duration = Duration::from_millis(90);

/// Fade in and out at each end of the tone.
///
/// A sine that starts and stops at full amplitude is a step change in the
/// waveform, which every speaker in the world renders as a click. Five
/// milliseconds is inaudible as a fade and completely removes it.
const FADE: Duration = Duration::from_millis(5);

/// Peak amplitude, well under full scale. This plays over whatever the user is
/// already listening to, and a dictation cue that ducks their music is worse
/// than no cue.
const AMPLITUDE: f32 = 0.15;

/// How long the player thread waits for the stream to finish before dropping
/// it. The tone plus a margin for the device's own buffer — dropping a `cpal`
/// stream stops the callback immediately, so cutting this fine would clip the
/// tail.
const DRAIN: Duration = Duration::from_millis(120);

/// Which end of the take is sounding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Start. The higher of the two, rising against the stop tone, because a
    /// pair the user cannot tell apart says nothing about which one happened.
    Start,
    Stop,
}

impl Tone {
    /// Hertz. Both sit in the range a laptop speaker reproduces honestly —
    /// low enough not to be shrill, high enough to cut through speech.
    fn frequency(self) -> f32 {
        match self {
            Tone::Start => 880.0,
            Tone::Stop => 587.0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tone::Start => "start",
            Tone::Stop => "stop",
        }
    }
}

/// Plays `tone`, on a thread of its own.
///
/// Returns immediately. Opening an output device can take a moment on WASAPI —
/// the same reason [`crate::audio::Capture`] bounds its own start — and the
/// callers are the two moments in a take where a stall is most visible: the
/// key going down, and the socket being told to close.
///
/// Failures are logged and nothing else. This is a cue, not a feature.
pub fn play(tone: Tone) {
    let spawned = thread::Builder::new()
        .name("splaude-tone".into())
        .spawn(move || {
            if let Err(error) = sound(tone) {
                diagnostic::log(
                    "sound",
                    format!("could not play the {} tone: {error:#}", tone.label()),
                );
            }
        });

    if let Err(error) = spawned {
        diagnostic::log("sound", format!("could not start the tone thread: {error}"));
    }
}

/// Opens the default output, plays one tone, and closes it again.
///
/// The device is opened per tone rather than held for the life of the process.
/// A background app that keeps an output stream open is one that keeps the
/// audio device — and on some drivers the whole endpoint — busy while doing
/// nothing, which is exactly the complaint people have about apps like this.
fn sound(tone: Tone) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no output device available"))?;

    let supported = device.default_output_config()?;
    let rate = supported.sample_rate();
    let channel_count = supported.channels();
    let config = supported.config();

    let mut voice = Voice::new(tone.frequency(), rate, channel_count);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, voice),
        SampleFormat::I16 => build::<i16>(&device, config, voice),
        SampleFormat::U16 => build::<u16>(&device, config, voice),
        SampleFormat::I32 => build::<i32>(&device, config, voice),
        other => {
            // Not worth a conversion path of its own: the tone is a cue, and a
            // device that speaks only i24 still gets a silent one.
            voice.silence();
            Err(anyhow!(
                "output device speaks {other}, which this cannot fill"
            ))
        }
    }?;

    stream.play()?;
    thread::sleep(DRAIN);
    drop(stream);

    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut voice: Voice,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device.build_output_stream(
        config,
        move |block: &mut [T], _: &cpal::OutputCallbackInfo| {
            for slot in block.iter_mut() {
                *slot = T::from_sample(voice.next());
            }
        },
        |error| diagnostic::log("sound", format!("output stream error: {error}")),
        Some(DRAIN),
    )?;
    Ok(stream)
}

// MARK: - Synthesis

/// One tone, sample by sample.
///
/// Pure arithmetic over a frame counter, deliberately: the whole of what makes
/// this a tone rather than a click is testable with no output device, which is
/// the only way it can be tested at all in CI.
struct Voice {
    frequency: f32,
    rate: f32,
    /// Interleaved output means the same value goes to every channel of a
    /// frame, so the counter only advances once per frame.
    channel_count: u16,
    channel: u16,
    frame: u32,
    frame_count: u32,
    fade_frame: u32,
}

impl Voice {
    fn new(frequency: f32, rate: u32, channel_count: u16) -> Self {
        let rate = rate.max(1);
        Self {
            frequency,
            rate: rate as f32,
            channel_count: channel_count.max(1),
            channel: 0,
            frame: 0,
            frame_count: frame_of(LENGTH, rate),
            fade_frame: frame_of(FADE, rate),
        }
    }

    /// Turns the tone into silence, for a device this cannot fill.
    fn silence(&mut self) {
        self.frame_count = 0;
    }

    fn next(&mut self) -> f32 {
        let value = envelope(self.frame, self.frame_count, self.fade_frame)
            * AMPLITUDE
            * (TAU * self.frequency * self.frame as f32 / self.rate).sin();

        self.channel += 1;
        if self.channel >= self.channel_count {
            self.channel = 0;
            // Saturating: the stream outlives the tone by the drain margin, and
            // an overflow would restart it rather than stay silent.
            self.frame = self.frame.saturating_add(1);
        }

        value
    }
}

fn frame_of(length: Duration, rate: u32) -> u32 {
    (length.as_secs_f64() * f64::from(rate)) as u32
}

/// Amplitude scale at `frame`: zero before the attack and after the end, one in
/// the middle, linear across each fade.
///
/// Pure and standalone so the click-free property — starts at silence, ends at
/// silence, never exceeds unity — is a test rather than a claim.
fn envelope(frame: u32, frame_count: u32, fade_frame: u32) -> f32 {
    if frame >= frame_count {
        return 0.0;
    }
    if fade_frame == 0 {
        return 1.0;
    }

    let fade = fade_frame as f32;
    let rising = frame as f32 / fade;
    // `frame < frame_count` above, so this cannot underflow.
    let falling = (frame_count - frame) as f32 / fade;

    rising.min(falling).clamp(0.0, 1.0)
}

#[cfg(test)]
mod test {
    use super::*;

    // No test here opens an output device: CI has no sound card, and a
    // developer machine running these should not beep at its owner.

    /// A plausible device: 48 kHz stereo.
    const RATE: u32 = 48_000;

    fn voice() -> Voice {
        Voice::new(Tone::Start.frequency(), RATE, 2)
    }

    #[test]
    fn the_two_tone_are_distinguishable() {
        assert_ne!(Tone::Start.frequency(), Tone::Stop.frequency());
        assert!(Tone::Start.frequency() > Tone::Stop.frequency());
    }

    #[test]
    fn the_envelope_opens_and_closes_at_silence() {
        let count = frame_of(LENGTH, RATE);
        let fade = frame_of(FADE, RATE);

        // The click this exists to prevent is a waveform that starts or stops
        // at full amplitude.
        assert_eq!(envelope(0, count, fade), 0.0);
        assert_eq!(envelope(count, count, fade), 0.0);
        assert_eq!(envelope(count + 1_000, count, fade), 0.0);
        // And full in the middle, or the tone would be inaudible.
        assert_eq!(envelope(count / 2, count, fade), 1.0);
    }

    #[test]
    fn the_envelope_never_exceeds_unity() {
        let count = frame_of(LENGTH, RATE);
        let fade = frame_of(FADE, RATE);
        for frame in 0..count + 100 {
            let level = envelope(frame, count, fade);
            assert!((0.0..=1.0).contains(&level), "frame {frame} at {level}");
        }
    }

    #[test]
    fn a_rate_so_low_there_is_no_fade_still_yields_a_tone() {
        // `frame_of` truncates, so a pathological device could report a fade of
        // zero frames. Dividing by it would be a NaN in the audio callback.
        assert_eq!(envelope(0, 4, 0), 1.0);
        assert_eq!(envelope(4, 4, 0), 0.0);
    }

    #[test]
    fn every_sample_stays_inside_full_scale() {
        // Anything outside this clips on the way to the speaker, which is the
        // click the envelope exists to avoid, arriving by another route.
        let mut voice = voice();
        for _ in 0..RATE {
            let sample = voice.next();
            assert!(sample.abs() <= AMPLITUDE, "{sample} exceeds the amplitude");
        }
    }

    #[test]
    fn the_tone_falls_silent_and_stays_silent() {
        let mut voice = voice();
        let frame_count = voice.frame_count;

        // Past the end of the tone the stream is still running — the drain
        // margin outlasts it — and must be feeding the device zeroes.
        for _ in 0..(frame_count as usize + 10) * 2 {
            voice.next();
        }
        for _ in 0..1_000 {
            assert_eq!(voice.next(), 0.0);
        }
    }

    #[test]
    fn a_frame_reaches_every_channel_before_the_phase_moves() {
        // Interleaved output: both channels of a frame carry the same value, or
        // the two speakers would be a sample apart and the tone would beat.
        let mut voice = Voice::new(880.0, RATE, 2);
        // Start a little way in, past the silent first frame.
        for _ in 0..200 {
            voice.next();
        }
        let left = voice.next();
        let right = voice.next();
        assert_eq!(left, right);
    }

    #[test]
    fn a_mono_device_advances_every_sample() {
        let mut voice = Voice::new(880.0, RATE, 1);
        voice.next();
        assert_eq!(voice.frame, 1);
    }

    #[test]
    fn a_device_reporting_nothing_usable_does_not_divide_by_zero() {
        // `channel_count` and `rate` come off the driver, and a zero in either
        // would be an infinite loop or a NaN in the audio callback.
        let mut voice = Voice::new(880.0, 0, 0);
        for _ in 0..64 {
            assert!(voice.next().is_finite());
        }
    }

    #[test]
    fn a_silenced_voice_emits_nothing() {
        let mut voice = voice();
        voice.silence();
        for _ in 0..1_000 {
            assert_eq!(voice.next(), 0.0);
        }
    }
}
