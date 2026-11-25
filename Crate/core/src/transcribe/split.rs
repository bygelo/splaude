//! Where to cut a long recording so its pieces can be transcribed at once.
//!
//! Timings are only trustworthy when audio is fed at the speed it was spoken
//! (see [`super::word`]), so an hour of video through one socket is an hour of
//! waiting. Cutting it into pieces and running several sockets side by side
//! keeps the 1× pacing and divides the wall clock. A cut through a word loses
//! the word, so each cut is moved to the quietest stretch near where it was
//! planned.

use super::word::Envelope;

/// Shorter than this and the handshake is a large share of the piece.
pub const MIN_CHUNK_MS: u64 = 30_000;
/// Longer than this and one failed socket costs a lot to redo.
pub const MAX_CHUNK_MS: u64 = 240_000;
/// How long a stretch has to be quiet to cut in.
const CUT_WINDOW_MS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Plans pieces of roughly `duration / parallel` each, every cut placed in the
/// quietest window within reach of where it would naturally fall.
pub fn plan(envelope: &Envelope, parallel: usize) -> Vec<Chunk> {
    let duration = envelope.duration_ms();
    let target = (duration / parallel.max(1) as u64).clamp(MIN_CHUNK_MS, MAX_CHUNK_MS);
    if duration <= target + target / 2 {
        return vec![Chunk {
            start_ms: 0,
            end_ms: duration,
        }];
    }

    let count = duration.div_ceil(target);
    let length = duration / count;
    let reach = (length / 4).min(15_000);

    let mut cut = Vec::new();
    for index in 1..count {
        let nominal = index * length;
        let from = nominal.saturating_sub(reach);
        let to = (nominal + reach).min(duration.saturating_sub(CUT_WINDOW_MS));
        let mut best = (nominal, f32::MAX);
        let mut at = from;
        while at <= to {
            let level = envelope.mean(at, at + CUT_WINDOW_MS);
            if level < best.1 {
                best = (at + CUT_WINDOW_MS / 2, level);
            }
            at += 50;
        }
        cut.push(best.0);
    }

    let mut chunk = Vec::new();
    let mut start = 0;
    for end in cut.into_iter().chain(std::iter::once(duration)) {
        if end > start {
            chunk.push(Chunk {
                start_ms: start,
                end_ms: end,
            });
            start = end;
        }
    }
    chunk
}

#[cfg(test)]
mod test {
    use super::*;

    fn speech_with_gap(total_ms: u64, gap: &[u64]) -> Vec<i16> {
        (0..total_ms * 16)
            .map(|index| {
                let ms = index / 16;
                if gap.iter().any(|at| ms >= *at && ms < at + 600) {
                    0
                } else {
                    ((index as f64 * 0.3).sin() * 8_000.0) as i16
                }
            })
            .collect()
    }

    #[test]
    fn a_short_file_is_one_piece() {
        let envelope = Envelope::of(&speech_with_gap(20_000, &[]));
        assert_eq!(
            plan(&envelope, 8),
            [Chunk {
                start_ms: 0,
                end_ms: 20_000
            }]
        );
    }

    #[test]
    fn pieces_cover_the_file_without_gaps_and_cut_in_silence() {
        // Ten minutes, with the only pauses a few seconds off each nominal cut.
        let gap = [
            62_000, 118_000, 183_000, 238_000, 303_000, 357_000, 421_000, 478_000, 541_000,
        ];
        let envelope = Envelope::of(&speech_with_gap(600_000, &gap));
        let chunk = plan(&envelope, 10);
        assert_eq!(chunk.first().unwrap().start_ms, 0);
        assert_eq!(chunk.last().unwrap().end_ms, 600_000);
        for pair in chunk.windows(2) {
            assert_eq!(pair[0].end_ms, pair[1].start_ms);
        }
        for piece in &chunk[..chunk.len() - 1] {
            let cut = piece.end_ms;
            assert!(
                gap.iter().any(|at| cut >= *at && cut < at + 600),
                "cut at {cut} is not in a pause"
            );
        }
    }
}
