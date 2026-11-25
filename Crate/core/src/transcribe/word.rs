//! Word timings from a recogniser that sends none.
//!
//! The Anthropic endpoint answers with text only — `{"type":"TranscriptText",
//! "data":"Hello there. This"}`, cumulative within an utterance, and no
//! offsets. What it does have is latency that is small and steady when audio
//! is fed at the speed it was recorded: measured against the live endpoint, a
//! new word shows up about a second after it was spoken, give or take a few
//! tenths. So the moment a
//! word first appears, read against how much audio had been sent by then, is a
//! usable estimate of where it ends, once the recogniser's steady ~0.95 s of
//! look-ahead is taken off.
//!
//! That is only true at 1×. Fed at 3× or 10× the final text is still right but
//! the server decodes at its own pace and arrival stops tracking the audio, so
//! the file path paces every chunk like a microphone and buys speed with
//! parallel sockets instead.
//!
//! [`Tracker`] turns the stream of cumulative texts into words with an
//! estimated end; [`refine`] then snaps both edges to the audio's own speech
//! energy, which is where most of the precision comes from.

use serde::Serialize;

/// How far behind the audio a word's first appearance typically lands. Taken
/// off every arrival so an estimate is centred rather than always late.
///
/// Measured, not guessed, against a synthetic video with known word
/// boundaries (167 words, four parallel sockets, live endpoint), with
/// `SPLAUDE_RAW_TIMING=1` so the energy snapping could not hide the error:
/// assuming 1100 ms, raw word ends came back a mean 150 ms early on clean
/// speech and 180 ms early under pink noise — so about 950 ms. The first
/// probe's "0.1–0.3 s" compared the text with audio *sent*, which hides the
/// recogniser's own look-ahead.
pub const ARRIVAL_LATENCY_MS: u64 = 950;

/// The longest a burst of words arriving together is assumed to span. A
/// burst after a long silence must not be spread across the silence.
const BURST_WORD_MS: u64 = 350;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Word {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Turns the recogniser's cumulative text into timed words.
///
/// Positions are milliseconds of audio sent when the text arrived, relative to
/// the start of whatever was sent — the caller adds the chunk's offset.
#[derive(Debug, Default)]
pub struct Tracker {
    committed: Vec<Word>,
    current: Vec<Word>,
    /// When the last update that added a word arrived.
    last_growth_ms: u64,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one transcript event.
    ///
    /// Within an utterance each text replaces the last. A word at an index the
    /// previous text already had keeps its time even if its spelling changed —
    /// that is the recogniser revising a word it already heard ("Splod" became
    /// "Claude" in the measured run), not a new word. Words past the old end
    /// are new and are timed now.
    pub fn apply(&mut self, text: &str, is_final: bool, at_ms: u64) {
        let token: Vec<&str> = text.split_whitespace().collect();

        if token.len() < self.current.len() {
            self.current.truncate(token.len());
        }
        for (word, spelling) in self.current.iter_mut().zip(&token) {
            if word.text != *spelling {
                word.text = (*spelling).to_string();
            }
        }

        let fresh = &token[self.current.len()..];
        if !fresh.is_empty() {
            let end = at_ms.saturating_sub(ARRIVAL_LATENCY_MS);
            let floor = self.last_end().max(
                // The burst cannot have started before the last growth, and
                // is assumed to be no longer than a few words' worth of speech.
                end.saturating_sub(BURST_WORD_MS * fresh.len() as u64)
                    .max(self.last_growth_ms.saturating_sub(ARRIVAL_LATENCY_MS)),
            );
            let floor = floor.min(end);
            let count = fresh.len() as u64;
            for (index, spelling) in fresh.iter().enumerate() {
                let index = index as u64 + 1;
                let word_end = floor + (end - floor) * index / count;
                self.current.push(Word {
                    text: (*spelling).to_string(),
                    start_ms: word_end,
                    end_ms: word_end,
                });
            }
            self.last_growth_ms = at_ms;
        }

        if is_final {
            self.committed.append(&mut self.current);
        }
    }

    fn last_end(&self) -> u64 {
        self.current
            .last()
            .or(self.committed.last())
            .map(|word| word.end_ms)
            .unwrap_or(0)
    }

    /// Every word heard, committed or not. Starts are not yet estimated —
    /// that is [`refine`]'s job.
    pub fn finish(mut self) -> Vec<Word> {
        self.committed.append(&mut self.current);
        self.committed
    }
}

/// Per-frame loudness of the audio, for snapping word edges to speech.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// Decibels per [`Envelope::FRAME_MS`] frame.
    level: Vec<f32>,
    /// Above this, a frame is speech.
    threshold: f32,
}

impl Envelope {
    pub const FRAME_MS: u64 = 10;

    /// 16 kHz mono samples.
    pub fn of(sample: &[i16]) -> Self {
        let frame = (16 * Self::FRAME_MS) as usize;
        let level: Vec<f32> = sample
            .chunks(frame)
            .map(|chunk| {
                let power = chunk
                    .iter()
                    .map(|value| (*value as f64 / 32768.0).powi(2))
                    .sum::<f64>()
                    / chunk.len().max(1) as f64;
                (10.0 * (power + 1e-10).log10()) as f32
            })
            .collect();

        // The quiet tenth of the file is taken as its floor, and speech as
        // anything a clear margin above it. A fixed threshold would call a
        // whole music bed speech, or a whispered aside silence.
        let mut sorted = level.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let floor = sorted
            .get(sorted.len() / 10)
            .copied()
            .unwrap_or(-100.0)
            .max(-70.0);
        let peak = sorted.get(sorted.len() * 95 / 100).copied().unwrap_or(0.0);
        let threshold = (floor + 12.0).min(peak - 10.0).max(floor + 4.0);

        Self { level, threshold }
    }

    pub fn duration_ms(&self) -> u64 {
        self.level.len() as u64 * Self::FRAME_MS
    }

    pub(super) fn is_speech(&self, ms: u64) -> bool {
        self.level
            .get((ms / Self::FRAME_MS) as usize)
            .is_some_and(|level| *level >= self.threshold)
    }

    /// Mean level over a window, for choosing where to cut.
    pub fn mean(&self, from_ms: u64, to_ms: u64) -> f32 {
        let from = (from_ms / Self::FRAME_MS) as usize;
        let to = ((to_ms / Self::FRAME_MS) as usize).min(self.level.len());
        if to <= from {
            return f32::MAX;
        }
        self.level[from..to].iter().sum::<f32>() / (to - from) as f32
    }
}

/// Silence this long between two words is a real pause, not a stop consonant.
const PAUSE_MS: u64 = 120;
/// How far an end estimate may be pushed later to reach the end of speech.
const END_REACH_MS: u64 = 200;
/// An end estimate in silence is pulled back to the last speech, but no
/// further than this past it.
const END_WALK_MS: u64 = 100;

/// Estimated length of a spoken word from its spelling. Only used where the
/// audio offers nothing better — continuous speech, or speech over music.
pub(super) fn spoken_ms(text: &str) -> u64 {
    let letter = text.chars().filter(|c| c.is_alphanumeric()).count() as u64;
    (90 + letter * 65).clamp(150, 1200)
}

/// Snaps each word's edges to the speech in `envelope`, and estimates starts.
///
/// Times are in the envelope's own frame — chunk-relative, like the tracker's.
pub fn refine(word: &mut [Word], envelope: &Envelope) {
    let duration = envelope.duration_ms();
    let mut previous_end = 0u64;

    for item in word.iter_mut() {
        let mut end = item.end_ms.min(duration).max(previous_end);

        // Where the speech of this word last sounded; `end` itself may sit
        // after it, in a tail the envelope cannot hear.
        let mut sounded = end;
        if envelope.is_speech(end) {
            // Still talking at the estimate: the word may run slightly past it.
            let limit = (end + END_REACH_MS).min(duration);
            while end < limit && envelope.is_speech(end + Envelope::FRAME_MS) {
                end += Envelope::FRAME_MS;
            }
            sounded = end;
        } else {
            // Estimate landed in silence after the word: walk back to its end.
            // Only a short way, though — under noise a word's quiet tail reads
            // as silence, and an unbounded walk cut ~190 ms off every word.
            let mut probe = end;
            while probe > previous_end && !envelope.is_speech(probe) {
                probe = probe.saturating_sub(Envelope::FRAME_MS);
            }
            if probe > previous_end {
                sounded = probe + Envelope::FRAME_MS;
                end = end.min(sounded + END_WALK_MS);
            }
        }

        // The start is the end of the last real pause before the word, if
        // there is one within reach; otherwise the word abuts the one before
        // it, bounded by how long a word that size takes to say.
        let reach = sounded
            .saturating_sub(spoken_ms(&item.text) * 2)
            .max(previous_end);
        let mut start = reach;
        let mut silent_run = 0u64;
        let mut probe = sounded;
        while probe > reach {
            probe = probe.saturating_sub(Envelope::FRAME_MS).max(reach);
            if envelope.is_speech(probe) {
                if silent_run >= PAUSE_MS {
                    break;
                }
                silent_run = 0;
            } else {
                silent_run += Envelope::FRAME_MS;
                if silent_run >= PAUSE_MS {
                    start = probe + silent_run;
                }
            }
        }
        if silent_run < PAUSE_MS {
            start = start.max(sounded.saturating_sub(spoken_ms(&item.text)));
        }
        // A word squeezed against the previous one, or against the end of the
        // audio, may be left with no length; it never runs backwards.
        let start = start.max(previous_end).min(end);

        item.start_ms = start;
        item.end_ms = end;
        previous_end = item.end_ms;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn text(word: &[Word]) -> Vec<&str> {
        word.iter().map(|w| w.text.as_str()).collect()
    }

    #[test]
    fn a_new_word_is_timed_at_its_arrival_less_the_latency() {
        let mut tracker = Tracker::new();
        tracker.apply("Hello", false, 1_200);
        let word = tracker.finish();
        assert_eq!(word[0].end_ms, 1_200 - ARRIVAL_LATENCY_MS);
    }

    #[test]
    fn a_revised_word_keeps_its_time() {
        let mut tracker = Tracker::new();
        tracker.apply("test of the Splod", false, 3_400);
        tracker.apply("test of the Claude", false, 15_000);
        let word = tracker.finish();
        assert_eq!(text(&word), ["test", "of", "the", "Claude"]);
        assert!(word[3].end_ms <= 3_400);
    }

    #[test]
    fn words_arriving_together_are_spread_not_stacked() {
        let mut tracker = Tracker::new();
        tracker.apply("one", false, 1_000);
        tracker.apply("one two three", false, 2_000);
        let word = tracker.finish();
        assert!(word[1].end_ms < word[2].end_ms);
        assert!(word[1].end_ms > word[0].end_ms);
    }

    #[test]
    fn a_burst_after_a_long_silence_is_not_spread_across_it() {
        let mut tracker = Tracker::new();
        tracker.apply("before", true, 1_000);
        tracker.apply("after the pause", false, 20_000);
        let word = tracker.finish();
        assert!(word[1].end_ms >= 20_000 - ARRIVAL_LATENCY_MS - 3 * BURST_WORD_MS);
    }

    #[test]
    fn a_final_commits_and_the_next_utterance_starts_fresh() {
        let mut tracker = Tracker::new();
        tracker.apply("first utterance", false, 1_000);
        tracker.apply("first utterance.", true, 1_500);
        tracker.apply("second", false, 3_000);
        let word = tracker.finish();
        assert_eq!(text(&word), ["first", "utterance.", "second"]);
    }

    #[test]
    fn a_shorter_revision_drops_the_tail() {
        let mut tracker = Tracker::new();
        tracker.apply("gonna go there", false, 1_000);
        tracker.apply("going there", true, 1_200);
        assert_eq!(text(&tracker.finish()), ["going", "there"]);
    }

    /// 16 kHz samples: silence, then a tone between `from` and `to` ms.
    fn tone(total_ms: u64, burst: &[(u64, u64)]) -> Vec<i16> {
        (0..total_ms * 16)
            .map(|index| {
                let ms = index / 16;
                if burst.iter().any(|(from, to)| ms >= *from && ms < *to) {
                    ((index as f64 * 0.3).sin() * 8_000.0) as i16
                } else {
                    0
                }
            })
            .collect()
    }

    #[test]
    fn edges_snap_to_the_speech_around_the_estimate() {
        let envelope = Envelope::of(&tone(5_000, &[(1_000, 1_600), (2_500, 3_000)]));
        let mut word = vec![
            Word {
                text: "hello".into(),
                start_ms: 1_800,
                end_ms: 1_800,
            },
            Word {
                text: "there".into(),
                start_ms: 2_900,
                end_ms: 2_900,
            },
        ];
        refine(&mut word, &envelope);
        // The first estimate landed in the silence after the word: pulled
        // back, but only to within END_WALK_MS of the last speech — that
        // "silence" is often a tail too quiet for the envelope.
        assert_eq!(word[0].end_ms, 1_600 + END_WALK_MS, "{:?}", word[0]);
        assert!((990..=1_020).contains(&word[0].start_ms), "{:?}", word[0]);
        // The second was early, inside the word, and reaches its end.
        assert!((2_990..=3_020).contains(&word[1].end_ms), "{:?}", word[1]);
        assert!((2_490..=2_520).contains(&word[1].start_ms), "{:?}", word[1]);
    }

    #[test]
    fn words_never_overlap_or_run_backwards() {
        let envelope = Envelope::of(&tone(3_000, &[(0, 3_000)]));
        let mut word = vec![
            Word {
                text: "a".into(),
                start_ms: 900,
                end_ms: 900,
            },
            Word {
                text: "b".into(),
                start_ms: 800,
                end_ms: 800,
            },
            Word {
                text: "c".into(),
                start_ms: 5_000,
                end_ms: 5_000,
            },
        ];
        refine(&mut word, &envelope);
        for pair in word.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms, "{word:?}");
        }
        for item in &word {
            assert!(item.start_ms <= item.end_ms, "{item:?}");
            assert!(item.end_ms <= 3_000);
        }
    }
}
