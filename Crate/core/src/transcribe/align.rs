//! Re-timing the phrases the server was late on.
//!
//! [`super::word::refine`] trusts each word's arrival time minus a calibrated
//! latency, then snaps it to the audio. That holds while the server keeps
//! pace. It does not always: in a measured 7-minute run two of five sessions
//! fell 300–800 ms behind partway through and stayed behind (the same pieces
//! in four runs out of four), and a sentence's last word there arrived up to
//! 2.2 s late — the recogniser holds it until it is sure the sentence ended.
//! Snapped from those arrivals, a sentence-final word lands at the start of
//! the next phrase, seconds off.
//!
//! Arrival times still say which phrase a word belongs to, if not exactly
//! when. So the audio is cut into phrases — speech between pauses of at least
//! [`PHRASE_GAP_MS`] — and the words, in order, are assigned to phrases by
//! dynamic programming: a phrase's words should take about as long to say as
//! the phrase lasts, should have arrived a plausible time after it, and should
//! not run across a sentence end. Where every word `refine` placed lies inside
//! its assigned phrase, `refine` stands. Where one does not, the phrase's
//! words are spread across it by length, with boundaries on its silences.

use super::cue::ends_sentence;
use super::word::{refine, spoken_ms, Envelope, Word};

/// Silence this long separates two phrases.
const PHRASE_GAP_MS: u64 = 250;
/// A burst of sound shorter than this is a click, not a phrase.
const PHRASE_MIN_MS: u64 = 60;
/// Silence inside a phrase this long may be a word boundary.
const WORD_GAP_MS: u64 = 30;
/// The most words one phrase is allowed to hold.
const RUN_MAX: usize = 60;
/// Cost of a sentence end inside a phrase rather than at its end.
const SENTENCE_COST: f64 = 2.0;
/// Cost of a word boundary that falls on no silence.
const GAPLESS_COST: f64 = 5.0;
/// Cost of a silence inside a phrase that no word boundary uses.
const UNUSED_GAP_COST: f64 = 1.0;
/// How early (negative) or late a phrase's last word may plausibly arrive
/// relative to the phrase's end, beyond the calibrated latency.
const LAG_EARLY_MS: f64 = -400.0;
const LAG_LATE_MS: f64 = 3_500.0;
/// A word shorter than this share of its spoken length was squeezed. Swept
/// 0–0.5 over four captured runs: 0.2 lifts the two long ones 88→91% and
/// 83→89% of starts within 250 ms and leaves the clean and noisy ones as they were.
const SQUEEZE: f64 = 0.2;
/// A phrase lag smaller than this is measurement noise (reported only).
const LAG_NOISE_MS: i64 = 100;
/// A word `refine` placed this far outside its phrase still counts as in it.
const SLACK_MS: u64 = 150;
/// More phrases than this per word means the envelope is tracking noise,
/// not speech: measured 104 phrases for 164 words under pink noise, against
/// 16–23 per 110–150 words on clean speech. Alignment is skipped then.
const PHRASE_PER_WORD_MAX: f64 = 0.4;
/// …once there are enough phrases for the ratio to mean anything.
const NOISE_PHRASE_MIN: usize = 8;

/// What [`retime`] did, for the log.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub phrase_count: usize,
    pub retimed: usize,
    /// Phrases whose arrivals were corrected by more than [`LAG_NOISE_MS`].
    pub lag_corrected: usize,
    /// The envelope did not look like speech and nothing was changed.
    pub is_noise: bool,
}

/// Spans of speech separated by at least [`PHRASE_GAP_MS`] of silence.
fn phrase_of(envelope: &Envelope) -> Vec<(u64, u64)> {
    let frame = Envelope::FRAME_MS;
    let total = envelope.duration_ms() / frame;
    let mut phrase = Vec::new();
    let mut open: Option<u64> = None;
    let mut last = 0u64;
    for index in 0..total {
        if !envelope.is_speech(index * frame) {
            continue;
        }
        match open {
            None => open = Some(index),
            Some(start) if (index - last - 1) * frame >= PHRASE_GAP_MS => {
                phrase.push((start * frame, (last + 1) * frame));
                open = Some(index);
            }
            Some(_) => {}
        }
        last = index;
    }
    if let Some(start) = open {
        phrase.push((start * frame, (last + 1) * frame));
    }
    phrase.retain(|(start, end)| end - start >= PHRASE_MIN_MS);
    phrase
}

/// A run of words `from..to` assigned to phrase `phrase`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    phrase: usize,
    from: usize,
    to: usize,
}

/// Assigns contiguous runs of words to phrases, in order. Every word lands in
/// some phrase; a phrase may hold none (a cough, a door).
fn assign(word: &[Word], raw_end: &[u64], phrase: &[(u64, u64)]) -> Vec<Run> {
    let n = word.len();
    let m = phrase.len();
    // Prefix sums of expected spoken length and of sentence ends.
    let mut length = vec![0f64; n + 1];
    let mut sentence = vec![0usize; n + 1];
    for (index, item) in word.iter().enumerate() {
        length[index + 1] = length[index] + spoken_ms(&item.text) as f64;
        sentence[index + 1] = sentence[index] + usize::from(ends_sentence(&item.text));
    }

    let at = |k: usize, i: usize| k * (n + 1) + i;
    let mut cost = vec![f64::INFINITY; (m + 1) * (n + 1)];
    let mut back = vec![usize::MAX; (m + 1) * (n + 1)];
    cost[at(0, 0)] = 0.0;

    for k in 1..=m {
        let (start, end) = phrase[k - 1];
        let span = (end - start) as f64;
        for i in 0..=n {
            // The phrase holds no words: unexplained sound costs by length.
            let empty = cost[at(k - 1, i)] + span / 300.0;
            if empty < cost[at(k, i)] {
                cost[at(k, i)] = empty;
                back[at(k, i)] = i;
            }
            for j in i.saturating_sub(RUN_MAX)..i {
                let before = cost[at(k - 1, j)];
                if !before.is_finite() {
                    continue;
                }
                let duration = ((length[i] - length[j]) / span).ln().abs() * 3.0;
                let lag = raw_end[i - 1] as f64 - end as f64;
                let lag_cost = if lag < LAG_EARLY_MS {
                    (LAG_EARLY_MS - lag) / 50.0
                } else if lag > LAG_LATE_MS {
                    (lag - LAG_LATE_MS) / 50.0
                } else {
                    lag.abs() / 1_500.0
                };
                // A sentence end anywhere but the run's last word.
                let inner = sentence[i - 1] - sentence[j];
                let total = before + duration + lag_cost + SENTENCE_COST * inner as f64;
                if total < cost[at(k, i)] {
                    cost[at(k, i)] = total;
                    back[at(k, i)] = j;
                }
            }
        }
    }

    let mut run = Vec::new();
    let mut i = n;
    for k in (1..=m).rev() {
        let j = back[at(k, i)];
        if j == usize::MAX {
            // Unreachable: no assignment of every word exists.
            return Vec::new();
        }
        if j < i {
            run.push(Run {
                phrase: k - 1,
                from: j,
                to: i,
            });
        }
        i = j;
    }
    if i != 0 {
        return Vec::new();
    }
    run.reverse();
    run
}

/// Spreads `word` over one phrase by expected length, putting each boundary
/// on a silence inside the phrase where one is near.
fn spread(word: &[Word], (start, end): (u64, u64), envelope: &Envelope) -> Vec<(u64, u64)> {
    let frame = Envelope::FRAME_MS;
    let mut gap: Vec<(u64, u64)> = Vec::new();
    let mut index = start / frame;
    while index < end / frame {
        if !envelope.is_speech(index * frame) {
            let from = index;
            while index < end / frame && !envelope.is_speech(index * frame) {
                index += 1;
            }
            if (index - from) * frame >= WORD_GAP_MS {
                gap.push((from * frame, index * frame));
            }
        }
        index += 1;
    }

    let total: u64 = word
        .iter()
        .map(|item| spoken_ms(&item.text))
        .sum::<u64>()
        .max(1);
    let mut said = 0u64;
    let estimate: Vec<f64> = word[..word.len().saturating_sub(1)]
        .iter()
        .map(|item| {
            said += spoken_ms(&item.text);
            start as f64 + (end - start) as f64 * said as f64 / total as f64
        })
        .collect();

    // Boundaries onto gaps in order: each boundary takes the next unused gap
    // or none; each gap is used once or skipped.
    let (b, g) = (estimate.len(), gap.len());
    let at = |x: usize, y: usize| x * (g + 1) + y;
    let mut cost = vec![f64::INFINITY; (b + 1) * (g + 1)];
    // 0 skip a gap, 1 no gap for this boundary, 2 this boundary on this gap.
    let mut step = vec![u8::MAX; (b + 1) * (g + 1)];
    cost[at(0, 0)] = 0.0;
    for x in 0..=b {
        for y in 0..=g {
            let here = cost[at(x, y)];
            if !here.is_finite() {
                continue;
            }
            if y < g && here + UNUSED_GAP_COST < cost[at(x, y + 1)] {
                cost[at(x, y + 1)] = here + UNUSED_GAP_COST;
                step[at(x, y + 1)] = 0;
            }
            if x < b {
                if here + GAPLESS_COST < cost[at(x + 1, y)] {
                    cost[at(x + 1, y)] = here + GAPLESS_COST;
                    step[at(x + 1, y)] = 1;
                }
                if y < g {
                    let middle = (gap[y].0 + gap[y].1) as f64 / 2.0;
                    let total = here + (middle - estimate[x]).abs() / 100.0;
                    if total < cost[at(x + 1, y + 1)] {
                        cost[at(x + 1, y + 1)] = total;
                        step[at(x + 1, y + 1)] = 2;
                    }
                }
            }
        }
    }
    let mut edge = vec![(0u64, 0u64); b];
    let (mut x, mut y) = (b, g);
    while x > 0 || y > 0 {
        match step[at(x, y)] {
            0 => y -= 1,
            1 => {
                x -= 1;
                let at = estimate[x].round() as u64;
                edge[x] = (at, at);
            }
            _ => {
                x -= 1;
                y -= 1;
                edge[x] = gap[y];
            }
        }
    }

    let mut out = Vec::with_capacity(word.len());
    let mut from = start;
    for index in 0..word.len() {
        let (word_end, next) = edge.get(index).copied().unwrap_or((end, end));
        let word_end = word_end.max(from);
        out.push((from, word_end));
        from = next.max(word_end);
    }
    out
}

/// Times the words of one piece. `word` comes in with the tracker's raw
/// arrival estimates and leaves with start and end times.
///
/// 1. Words are assigned to phrases from their arrivals.
/// 2. Each phrase's lag — how much later than the calibration its words
///    arrived, the median against a length-based spread over the phrase — is
///    taken off its words' arrivals, and `refine` snaps the result to the
///    audio. A phrase the server kept pace on measures about zero and is
///    unchanged.
/// 3. A phrase whose snapped words still stray outside it is spread over the
///    phrase instead.
pub fn time(word: &mut [Word], envelope: &Envelope) -> Outcome {
    let raw_end: Vec<u64> = word.iter().map(|item| item.end_ms).collect();
    let phrase = phrase_of(envelope);
    let mut outcome = Outcome {
        phrase_count: phrase.len(),
        ..Outcome::default()
    };
    let is_noise = phrase.len() >= NOISE_PHRASE_MIN
        && phrase.len() as f64 > PHRASE_PER_WORD_MAX * word.len() as f64;
    let run = if word.is_empty() || phrase.is_empty() || is_noise {
        Vec::new()
    } else {
        assign(word, &raw_end, &phrase)
    };
    outcome.is_noise = is_noise;
    if run.is_empty() {
        refine(word, envelope);
        return outcome;
    }

    let spread_of: Vec<Vec<(u64, u64)>> = run
        .iter()
        .map(|item| spread(&word[item.from..item.to], phrase[item.phrase], envelope))
        .collect();
    for (item, time) in run.iter().zip(&spread_of) {
        let mut lag: Vec<i64> = (item.from..item.to)
            .zip(time)
            .map(|(index, (_, end))| raw_end[index] as i64 - *end as i64)
            .collect();
        lag.sort_unstable();
        let lag = lag[lag.len() / 2];
        if lag.abs() > LAG_NOISE_MS {
            outcome.lag_corrected += 1;
        }
        for index in item.from..item.to {
            word[index].end_ms = (raw_end[index] as i64 - lag).max(0) as u64;
        }
    }
    // Corrections can cross at phrase edges; arrivals never run backwards.
    let mut floor = 0;
    for item in word.iter_mut() {
        item.end_ms = item.end_ms.max(floor);
        item.start_ms = item.end_ms;
        floor = item.end_ms;
    }
    refine(word, envelope);

    for (item, time) in run.iter().zip(spread_of) {
        let (start, end) = phrase[item.phrase];
        let is_inside = word[item.from..item.to].iter().all(|item| {
            let middle = (item.start_ms + item.end_ms) / 2;
            middle + SLACK_MS >= start && middle <= end + SLACK_MS
        });
        // Words that arrived in one burst are piled against an edge: inside
        // the phrase, but far shorter than they take to say.
        let is_squeezed = word[item.from..item.to].iter().any(|item| {
            let length = item.end_ms - item.start_ms;
            (length as f64) < SQUEEZE * spoken_ms(&item.text) as f64
        });
        if is_inside && !is_squeezed {
            continue;
        }
        for (item, (word_start, word_end)) in word[item.from..item.to].iter_mut().zip(time) {
            item.start_ms = word_start;
            item.end_ms = word_end;
        }
        outcome.retimed += 1;
    }

    // Keep words in order after a phrase moved.
    let mut floor = 0;
    for item in word.iter_mut() {
        item.start_ms = item.start_ms.max(floor);
        item.end_ms = item.end_ms.max(item.start_ms);
        floor = item.end_ms;
    }
    outcome
}

#[cfg(test)]
mod test {
    use super::*;

    const RATE: usize = 16;

    /// Audio with a loud tone over each `(start, end)` span, silence elsewhere.
    fn audio(span: &[(u64, u64)], total_ms: u64) -> Vec<i16> {
        let mut sample = vec![0i16; total_ms as usize * RATE];
        for (start, end) in span {
            let from = *start as usize * RATE;
            for (offset, value) in sample[from..*end as usize * RATE].iter_mut().enumerate() {
                *value = if (from + offset) % 16 < 8 {
                    8_000
                } else {
                    -8_000
                };
            }
        }
        sample
    }

    fn word(text: &str, start_ms: u64, end_ms: u64) -> Word {
        Word {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn pauses_split_phrases_and_clicks_are_ignored() {
        let envelope = Envelope::of(&audio(
            &[
                (1_000, 2_000),
                (2_100, 2_500),
                (3_000, 3_020),
                (4_000, 5_000),
            ],
            6_000,
        ));
        // 100 ms is a word gap, not a phrase gap; the 20 ms click is dropped.
        assert_eq!(phrase_of(&envelope), [(1_000, 2_500), (4_000, 5_000)]);
    }

    /// Words carrying only raw arrival ends, as the tracker leaves them.
    fn arrived(text_end: &[(&str, u64)]) -> Vec<Word> {
        text_end
            .iter()
            .map(|(text, end)| word(text, *end, *end))
            .collect()
    }

    #[test]
    fn a_late_sentence_end_is_pulled_back_into_its_phrase() {
        // Two sentences; the server sat on "there." until the second phrase
        // was under way.
        let sample = audio(
            &[
                (1_000, 1_400),
                (1_460, 1_900),
                (4_000, 4_400),
                (4_460, 4_900),
            ],
            6_000,
        );
        let mut timed = arrived(&[
            ("Hello", 1_400),
            ("there.", 4_300),
            ("Good", 4_700),
            ("night.", 4_900),
        ]);
        time(&mut timed, &Envelope::of(&sample));
        // Ends are what the alignment fixes; starts inside continuous speech
        // are refine's length estimate and only need to stay in the phrase.
        assert_eq!(timed[0].end_ms, 1_400, "{timed:?}");
        assert_eq!(timed[1].end_ms, 1_900, "{timed:?}");
        assert!(timed[1].start_ms >= 1_400, "{timed:?}");
        assert!(timed[2].start_ms >= 4_000, "{timed:?}");
    }

    #[test]
    fn a_phrase_that_arrived_late_as_a_whole_is_moved_back() {
        // Every word 600 ms later than the calibration — within its own
        // phrase, so only the lag measurement can catch it.
        let sample = audio(
            &[
                (1_000, 1_300),
                (1_360, 1_700),
                (1_760, 2_400),
                (5_000, 5_800),
            ],
            7_000,
        );
        let mut timed = arrived(&[
            ("one", 1_900),
            ("two", 2_300),
            ("three.", 3_000),
            ("Four.", 6_400),
        ]);
        let outcome = time(&mut timed, &Envelope::of(&sample));
        assert!(outcome.lag_corrected >= 1, "{outcome:?}");
        let end: Vec<u64> = timed.iter().map(|item| item.end_ms).collect();
        assert_eq!(end, [1_300, 1_700, 2_400, 5_800], "{timed:?}");
        assert!(timed[3].start_ms >= 5_000, "{timed:?}");
    }

    #[test]
    fn a_burst_piled_against_the_phrase_end_is_spread_over_it() {
        // "The old scheduler struggled" arrived together, after the phrase.
        let sample = audio(
            &[
                (1_000, 1_300),
                (1_340, 1_650),
                (1_700, 2_200),
                (2_250, 2_800),
            ],
            5_000,
        );
        let mut timed = arrived(&[
            ("The", 2_800),
            ("old", 2_800),
            ("scheduler", 2_800),
            ("struggled.", 2_800),
        ]);
        let outcome = time(&mut timed, &Envelope::of(&sample));
        assert_eq!(outcome.retimed, 1, "{outcome:?}");
        assert!(
            timed[0].start_ms <= 1_100 && timed[0].end_ms <= 1_400,
            "{timed:?}"
        );
    }

    #[test]
    fn arrivals_that_kept_pace_are_only_snapped() {
        let sample = audio(&[(1_000, 1_400), (1_460, 1_900), (4_000, 4_900)], 6_000);
        let mut timed = arrived(&[("Hello", 1_400), ("there.", 1_900), ("Bye.", 4_900)]);
        let outcome = time(&mut timed, &Envelope::of(&sample));
        assert_eq!(
            (outcome.retimed, outcome.lag_corrected),
            (0, 0),
            "{outcome:?}"
        );
        let end: Vec<u64> = timed.iter().map(|item| item.end_ms).collect();
        assert_eq!(end, [1_400, 1_900, 4_900], "{timed:?}");
    }

    #[test]
    fn noise_is_not_mistaken_for_phrases() {
        // Forty separate bursts for four words: the envelope is not speech.
        let burst: Vec<(u64, u64)> = (0..40).map(|i| (i * 400, i * 400 + 80)).collect();
        let envelope = Envelope::of(&audio(&burst, 17_000));
        let mut timed = arrived(&[
            ("word", 300),
            ("word", 1_300),
            ("word", 2_300),
            ("word", 3_300),
        ]);
        let mut refined = timed.clone();
        refine(&mut refined, &envelope);
        let outcome = time(&mut timed, &envelope);
        assert!(outcome.is_noise);
        assert_eq!(timed, refined);
    }
}

/// Offline replay for tuning: `SPLAUDE_REPLAY_PCM` (16 kHz s16le),
/// `SPLAUDE_REPLAY_RAW` (a `SPLAUDE_RAW_TIMING=1` JSON), `SPLAUDE_REPLAY_PIECE`
/// ("0-76574,76574-165298,…"), `SPLAUDE_REPLAY_OUT`. Run with `--ignored`.
#[cfg(test)]
mod replay {
    use super::*;

    #[test]
    #[ignore]
    fn replay() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let byte = std::fs::read(var("SPLAUDE_REPLAY_PCM")).unwrap();
        let sample: Vec<i16> = byte
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(var("SPLAUDE_REPLAY_RAW")).unwrap())
                .unwrap();
        let all: Vec<Word> = serde_json::from_value::<Vec<serde_json::Value>>(raw["word"].clone())
            .unwrap()
            .into_iter()
            .map(|item| Word {
                text: item["text"].as_str().unwrap().into(),
                start_ms: item["end_ms"].as_u64().unwrap(),
                end_ms: item["end_ms"].as_u64().unwrap(),
            })
            .collect();
        let mut out = Vec::new();
        for piece in var("SPLAUDE_REPLAY_PIECE").split(',') {
            let (from, to) = piece.split_once('-').unwrap();
            let (from, to): (u64, u64) = (from.parse().unwrap(), to.parse().unwrap());
            let mut word: Vec<Word> = all
                .iter()
                .filter(|item| item.end_ms >= from && item.end_ms < to)
                .map(|item| Word {
                    text: item.text.clone(),
                    start_ms: item.start_ms - from,
                    end_ms: item.end_ms - from,
                })
                .collect();
            let slice = &sample[(from as usize * 16)..(to as usize * 16).min(sample.len())];
            let envelope = Envelope::of(slice);
            if std::env::var_os("SPLAUDE_REPLAY_REFINE_ONLY").is_some() {
                refine(&mut word, &envelope);
            } else {
                eprintln!("piece {from}: {:?}", time(&mut word, &envelope));
            }
            out.extend(word.into_iter().map(|item| Word {
                start_ms: item.start_ms + from,
                end_ms: item.end_ms + from,
                ..item
            }));
        }
        std::fs::write(
            var("SPLAUDE_REPLAY_OUT"),
            serde_json::json!({ "word": out }).to_string(),
        )
        .unwrap();
    }
}
