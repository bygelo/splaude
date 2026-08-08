//! Turns whatever the input device offers into the 16 kHz mono signed-16 PCM
//! the speech endpoint requires.
//!
//! `AVAudioConverter` did this on macOS and has no portable equivalent, so it
//! is hand-rolled — which is also why it is a plain data transform with tests
//! rather than something wired into the audio callback.
//!
//! Downsampling without a low-pass folds everything above 8 kHz back into the
//! speech band as aliasing, and sibilants land squarely there. So: mix to mono,
//! low-pass with a windowed-sinc FIR, then linearly interpolate onto the target
//! rate. Filter state carries across chunks — resetting it per callback would
//! click at every buffer boundary.

/// Odd so the filter has a whole-sample group delay.
const TAP_COUNT: usize = 33;

/// Cutoff as a fraction of the target rate. Slightly under Nyquist to leave the
/// transition band somewhere to go.
const CUTOFF_RATIO: f32 = 0.45;

pub struct Resampler {
    tap: Vec<f32>,
    /// Newest-last window of mono input, `tap.len()` long.
    history: Vec<f32>,
    /// Input-sample index of the next output, measured from `previous`.
    position: f64,
    /// Ratio of input samples per output sample.
    stride: f64,
    previous: f32,
    channel_count: usize,
}

impl Resampler {
    pub fn new(source_rate: u32, channel_count: u16, target_rate: u32) -> Self {
        let stride = f64::from(source_rate) / f64::from(target_rate);
        // Upsampling needs no anti-alias filter; a single unit tap is a no-op.
        let tap = if source_rate > target_rate {
            low_pass(CUTOFF_RATIO * target_rate as f32 / source_rate as f32)
        } else {
            vec![1.0]
        };

        Self {
            history: vec![0.0; tap.len()],
            tap,
            position: 0.0,
            stride,
            previous: 0.0,
            channel_count: channel_count.max(1) as usize,
        }
    }

    /// Feeds interleaved f32 samples and returns little-endian i16 bytes.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<u8> {
        let frame_count = interleaved.len() / self.channel_count;
        let mut out = Vec::with_capacity((frame_count as f64 / self.stride) as usize * 2 + 4);

        for frame in 0..frame_count {
            let start = frame * self.channel_count;
            let mono = interleaved[start..start + self.channel_count]
                .iter()
                .sum::<f32>()
                / self.channel_count as f32;

            self.history.rotate_left(1);
            *self.history.last_mut().expect("history is never empty") = mono;

            let filtered = self
                .tap
                .iter()
                .zip(self.history.iter())
                .map(|(tap, sample)| tap * sample)
                .sum::<f32>();

            // Emit every output that falls in [previous, filtered).
            while self.position < 1.0 {
                let blend = self.previous + (filtered - self.previous) * self.position as f32;
                out.extend_from_slice(&to_i16(blend).to_le_bytes());
                self.position += self.stride;
            }
            self.position -= 1.0;
            self.previous = filtered;
        }

        out
    }
}

/// Peak-normalised RMS of a buffer, mapped onto 0…1.
///
/// Ported from the Swift level meter: a useful speech range of -50 dBFS…0.
pub fn level_of(sample: &[f32]) -> f32 {
    if sample.is_empty() {
        return 0.0;
    }
    let sum: f32 = sample.iter().map(|value| value * value).sum();
    let rms = (sum / sample.len() as f32).sqrt();
    let decibel = 20.0 * rms.max(1e-7).log10();
    ((decibel + 50.0) / 50.0).clamp(0.0, 1.0)
}

/// Fast attack, slow release, so a meter reads as speech rather than noise.
pub fn smooth(current: f32, incoming: f32) -> f32 {
    if incoming > current {
        incoming
    } else {
        current * 0.8 + incoming * 0.2
    }
}

fn to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Hamming-windowed sinc, normalised to unity gain at DC.
fn low_pass(cutoff: f32) -> Vec<f32> {
    let middle = (TAP_COUNT / 2) as f32;
    let mut tap: Vec<f32> = (0..TAP_COUNT)
        .map(|index| {
            let offset = index as f32 - middle;
            let sinc = if offset == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * offset).sin()
                    / (std::f32::consts::PI * offset)
            };
            let window = 0.54
                - 0.46 * (2.0 * std::f32::consts::PI * index as f32 / (TAP_COUNT - 1) as f32).cos();
            sinc * window
        })
        .collect();

    let gain: f32 = tap.iter().sum();
    if gain.abs() > f32::EPSILON {
        for value in &mut tap {
            *value /= gain;
        }
    }
    tap
}

#[cfg(test)]
mod test {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    #[test]
    fn a_low_pass_has_unity_gain_at_dc() {
        let tap = low_pass(0.15);
        assert!((tap.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn silence_stays_silence() {
        let mut resampler = Resampler::new(48_000, 1, 16_000);
        let out = resampler.push(&vec![0.0; 4_800]);
        assert!(decode(&out).iter().all(|sample| *sample == 0));
    }

    #[test]
    fn output_length_follows_the_rate_ratio() {
        let mut resampler = Resampler::new(48_000, 1, 16_000);
        let out = resampler.push(&vec![0.0; 4_800]);
        // 4800 input frames at 48k → ~1600 output frames at 16k, 2 bytes each.
        let frame_count = out.len() / 2;
        assert!(
            (1595..=1605).contains(&frame_count),
            "got {frame_count} frames"
        );
    }

    #[test]
    fn a_non_integer_ratio_still_tracks_the_rate() {
        let mut resampler = Resampler::new(44_100, 1, 16_000);
        let out = resampler.push(&vec![0.0; 44_100]);
        let frame_count = out.len() / 2;
        assert!(
            (15_900..=16_100).contains(&frame_count),
            "got {frame_count} frames"
        );
    }

    #[test]
    fn stereo_is_mixed_down_to_mono() {
        let mut resampler = Resampler::new(16_000, 2, 16_000);
        // Left +1, right -1 cancels to silence; a channel-ignoring
        // implementation would pass +1 straight through.
        let interleaved: Vec<f32> = (0..1_000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let out = resampler.push(&interleaved);
        assert!(decode(&out).iter().all(|sample| sample.abs() < 16));
    }

    #[test]
    fn a_matched_rate_preserves_a_steady_signal() {
        let mut resampler = Resampler::new(16_000, 1, 16_000);
        let out = resampler.push(&vec![0.5; 2_000]);
        let decoded = decode(&out);
        // Skip the filter's warm-up, then it should sit at half scale.
        let settled = decoded[decoded.len() / 2];
        assert!(
            (settled - 16_383).abs() < 400,
            "settled at {settled}, expected ~16383"
        );
    }

    #[test]
    fn filter_state_carries_across_chunks() {
        let mut chunked = Resampler::new(48_000, 1, 16_000);
        let mut whole = Resampler::new(48_000, 1, 16_000);

        let signal = vec![0.4; 4_800];
        let mut from_chunk = Vec::new();
        for chunk in signal.chunks(480) {
            from_chunk.extend(chunked.push(chunk));
        }
        let from_whole = whole.push(&signal);

        // Splitting the input must not change a single output sample.
        assert_eq!(from_chunk, from_whole);
    }

    #[test]
    fn clipping_is_clamped_not_wrapped() {
        let mut resampler = Resampler::new(16_000, 1, 16_000);
        let out = resampler.push(&vec![9.0; 500]);
        let decoded = decode(&out);
        // A wrap would flip the sign; a clamp pins it at full scale.
        assert!(decoded[decoded.len() - 1] > 32_000);
    }

    #[test]
    fn level_maps_silence_low_and_full_scale_high() {
        assert_eq!(level_of(&[]), 0.0);
        assert!(level_of(&[0.0; 128]) < 0.01);
        assert!(level_of(&[1.0; 128]) > 0.99);
    }

    #[test]
    fn level_smoothing_attacks_fast_and_releases_slow() {
        // Jumps straight to a louder reading…
        assert_eq!(smooth(0.1, 0.9), 0.9);
        // …but eases back down.
        let released = smooth(0.9, 0.1);
        assert!(released > 0.1 && released < 0.9);
    }
}
