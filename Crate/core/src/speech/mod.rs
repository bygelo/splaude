//! A streaming speech-to-text session.
//!
//! Everything above this line is backend agnostic, so swapping in a direct
//! Deepgram key or a local model later means adding one file, not touching the
//! app. The Swift build expressed that with a delegate protocol; here it is a
//! channel of [`SpeechEvent`], which is the same contract without the
//! main-thread assumptions.

pub mod anthropic;

pub use anthropic::AnthropicSpeechBackend;

use std::time::Duration;

use tokio::sync::mpsc;

/// Audio contract the capture stage must satisfy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeechAudioFormat {
    pub sample_rate: u32,
    pub channel_count: u16,
}

impl SpeechAudioFormat {
    /// Signed 16-bit little-endian PCM. The only encoding any backend here
    /// wants.
    pub const LINEAR16_16K: Self = Self {
        sample_rate: 16_000,
        channel_count: 1,
    };

    /// Bytes one second of this format occupies. Signed 16-bit, so two bytes
    /// per sample per channel.
    pub const fn byte_rate(&self) -> u64 {
        self.sample_rate as u64 * self.channel_count as u64 * 2
    }

    /// How long `byte_count` bytes of this format take to speak.
    ///
    /// Exact rather than approximate: the encoding is fixed-width, so a byte
    /// count converts to a duration with no guessing. Anything reasoning about
    /// whether audio reached a server faster than it was spoken needs this.
    pub fn duration_of(&self, byte_count: usize) -> Duration {
        let rate = self.byte_rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(byte_count as u64 * 1_000_000_000 / rate)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpeechEvent {
    Open,
    /// `is_final` marks an utterance boundary — the text is committed at that
    /// point.
    Transcribe {
        text: String,
        is_final: bool,
    },
    Fail {
        message: String,
        fatal: bool,
    },
    Close,
}

/// What the app pushes into a running session.
#[derive(Debug)]
pub(crate) enum Frame {
    /// Raw PCM matching [`SpeechAudioFormat`].
    Audio(Vec<u8>),
    Close,
}

/// A live session. Dropping it ends the take.
#[derive(Debug, Clone)]
pub struct Session {
    frame: mpsc::UnboundedSender<Frame>,
}

impl Session {
    pub(crate) fn new(frame: mpsc::UnboundedSender<Frame>) -> Self {
        Self { frame }
    }

    /// Raw PCM matching the backend's audio format.
    ///
    /// Never blocks the capture callback — an audio thread that waits on a
    /// socket drops samples, so this queues and returns.
    pub fn send_audio(&self, pcm: Vec<u8>) {
        let _ = self.frame.send(Frame::Audio(pcm));
    }

    pub fn finish(&self) {
        let _ = self.frame.send(Frame::Close);
    }
}

pub trait SpeechBackend: Send + Sync {
    fn audio_format(&self) -> SpeechAudioFormat;
    /// Opens the connection and returns a handle to feed it. Events arrive on
    /// `event` until [`SpeechEvent::Close`].
    fn start(&self, event: mpsc::UnboundedSender<SpeechEvent>) -> anyhow::Result<Session>;
}

/// Mirrors the extension's committed/interim bookkeeping: finals accumulate
/// space-joined, and the live display is committed + the pending interim.
#[derive(Debug, Default, Clone)]
pub struct TranscriptBuffer {
    committed: String,
}

impl TranscriptBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed(&self) -> &str {
        &self.committed
    }

    /// Returns the full text to display after applying this chunk.
    pub fn apply(&mut self, text: &str, is_final: bool) -> String {
        let trimmed = text.trim();

        if trimmed.is_empty() {
            return self.committed.clone();
        }

        let joined = if self.committed.is_empty() {
            trimmed.to_string()
        } else {
            format!("{} {}", self.committed, trimmed)
        };

        if is_final {
            self.committed = joined.clone();
        }

        joined
    }

    pub fn reset(&mut self) {
        self.committed.clear();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_byte_count_converts_to_the_time_it_takes_to_speak() {
        let format = SpeechAudioFormat::LINEAR16_16K;
        assert_eq!(format.byte_rate(), 32_000);
        assert_eq!(format.duration_of(32_000), Duration::from_secs(1));
        assert_eq!(format.duration_of(1_024), Duration::from_micros(32_000));
        assert_eq!(format.duration_of(0), Duration::ZERO);
        // The size the truncation bug was first seen at: ~3.2 s in one burst.
        assert_eq!(
            format.duration_of(103_360),
            Duration::from_micros(3_230_000)
        );
    }

    #[test]
    fn an_interim_does_not_commit() {
        let mut buffer = TranscriptBuffer::new();
        assert_eq!(buffer.apply("hello", false), "hello");
        assert_eq!(buffer.committed(), "");
    }

    #[test]
    fn a_final_commits_and_accumulates_space_joined() {
        let mut buffer = TranscriptBuffer::new();
        assert_eq!(buffer.apply("one", true), "one");
        assert_eq!(buffer.apply("two", true), "one two");
        assert_eq!(buffer.committed(), "one two");
    }

    #[test]
    fn an_interim_displays_on_top_of_what_is_committed() {
        let mut buffer = TranscriptBuffer::new();
        buffer.apply("committed", true);
        assert_eq!(buffer.apply("pending", false), "committed pending");
        // …without becoming part of it.
        assert_eq!(buffer.committed(), "committed");
    }

    #[test]
    fn a_revised_interim_replaces_the_previous_one() {
        let mut buffer = TranscriptBuffer::new();
        buffer.apply("low testing", false);
        assert_eq!(buffer.apply("one two three", false), "one two three");
    }

    #[test]
    fn blank_and_whitespace_chunks_are_ignored() {
        let mut buffer = TranscriptBuffer::new();
        buffer.apply("kept", true);
        assert_eq!(buffer.apply("", true), "kept");
        assert_eq!(buffer.apply("   \n ", true), "kept");
        assert_eq!(buffer.committed(), "kept");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_before_joining() {
        let mut buffer = TranscriptBuffer::new();
        buffer.apply("  one  ", true);
        assert_eq!(buffer.apply("  two  ", true), "one two");
    }

    #[test]
    fn reset_clears_the_committed_text() {
        let mut buffer = TranscriptBuffer::new();
        buffer.apply("gone", true);
        buffer.reset();
        assert_eq!(buffer.apply("fresh", false), "fresh");
    }
}
