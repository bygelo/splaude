//! Timed words into the shapes people actually use: subtitle cues (SRT, VTT),
//! a readable timestamped transcript (TXT), and everything at once (JSON).

use serde::Serialize;

use super::word::Word;

/// Two subtitle lines of the broadcast-standard width.
const LINE_CHAR: usize = 42;
const CUE_CHAR: usize = LINE_CHAR * 2;
/// A cue on screen longer than this is hard to read along with.
const CUE_MAX_MS: u64 = 6_000;
/// A pause this long always starts a new cue.
const CUE_GAP_MS: u64 = 800;
/// A finished sentence ends a cue once the cue has been up this long.
const SENTENCE_MIN_MS: u64 = 1_200;
/// How long a cue may linger after its last word, so it is readable.
const HOLD_MS: u64 = 400;
/// A pause this long starts a new paragraph in the text transcript.
const PARAGRAPH_GAP_MS: u64 = 2_000;
/// …as does a paragraph growing past this many characters at a sentence end.
const PARAGRAPH_CHAR: usize = 600;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Titles and short forms whose period does not end the sentence.
const ABBREVIATION: [&str; 14] = [
    "dr.", "mr.", "mrs.", "ms.", "prof.", "st.", "jr.", "sr.", "vs.", "etc.", "e.g.", "i.e.",
    "no.", "mt.",
];

/// Whether a word closes its sentence ("there." yes, "Dr." no).
pub(super) fn ends_sentence(text: &str) -> bool {
    let is_closed =
        text.ends_with(['.', '!', '?']) || text.ends_with(".\"") || text.ends_with("?\"");
    is_closed && !ABBREVIATION.contains(&text.to_lowercase().as_str())
}

/// Groups words into subtitle cues.
pub fn cue_of(word: &[Word]) -> Vec<Cue> {
    let mut cue: Vec<Cue> = Vec::new();
    let mut current: Vec<&Word> = Vec::new();

    let flush = |current: &mut Vec<&Word>, cue: &mut Vec<Cue>| {
        if let (Some(first), Some(last)) = (current.first(), current.last()) {
            cue.push(Cue {
                start_ms: first.start_ms,
                end_ms: last.end_ms,
                text: current
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            });
        }
        current.clear();
    };

    for item in word {
        if let (Some(first), Some(last)) = (current.first(), current.last()) {
            let length: usize = current
                .iter()
                .map(|w| w.text.chars().count() + 1)
                .sum::<usize>()
                + item.text.chars().count();
            let is_too_long =
                length > CUE_CHAR || item.end_ms.saturating_sub(first.start_ms) > CUE_MAX_MS;
            let is_pause = item.start_ms.saturating_sub(last.end_ms) >= CUE_GAP_MS;
            let is_sentence_done = ends_sentence(&last.text)
                && last.end_ms.saturating_sub(first.start_ms) >= SENTENCE_MIN_MS;
            if is_too_long || is_pause || is_sentence_done {
                flush(&mut current, &mut cue);
            }
        }
        current.push(item);
    }
    flush(&mut current, &mut cue);

    // Linger a little after the last word, never into the next cue.
    for index in 0..cue.len() {
        let limit = cue
            .get(index + 1)
            .map(|next| next.start_ms)
            .unwrap_or(u64::MAX);
        cue[index].end_ms = (cue[index].end_ms + HOLD_MS)
            .min(limit)
            .max(cue[index].end_ms);
    }
    cue
}

/// Splits a cue's text into at most two balanced lines.
fn wrap(text: &str) -> String {
    if text.chars().count() <= LINE_CHAR {
        return text.to_string();
    }
    let word: Vec<&str> = text.split(' ').collect();
    let total = text.chars().count();
    let mut best = (usize::MAX, 1);
    let mut left = 0;
    for index in 1..word.len() {
        left += word[index - 1].chars().count() + usize::from(index > 1);
        let right = total - left - 1;
        let score = left.abs_diff(right);
        if score < best.0 {
            best = (score, index);
        }
    }
    format!("{}\n{}", word[..best.1].join(" "), word[best.1..].join(" "))
}

/// `HH:MM:SS` followed by `separator` and milliseconds.
fn clock(ms: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1_000 % 60,
        ms % 1_000
    )
}

/// `[MM:SS]`, or `[H:MM:SS]` past the hour.
fn stamp(ms: u64) -> String {
    let second = ms / 1_000;
    if second >= 3_600 {
        format!(
            "[{}:{:02}:{:02}]",
            second / 3_600,
            second / 60 % 60,
            second % 60
        )
    } else {
        format!("[{:02}:{:02}]", second / 60, second % 60)
    }
}

pub fn srt(cue: &[Cue]) -> String {
    let mut out = String::new();
    for (index, item) in cue.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            clock(item.start_ms, ','),
            clock(item.end_ms, ','),
            wrap(&item.text)
        ));
    }
    out
}

pub fn vtt(cue: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for item in cue {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            clock(item.start_ms, '.'),
            clock(item.end_ms, '.'),
            wrap(&item.text)
        ));
    }
    out
}

/// Readable paragraphs, each led by the time it starts.
pub fn txt(cue: &[Cue]) -> String {
    let mut out = String::new();
    let mut paragraph = String::new();
    let mut previous_end = 0;

    for item in cue {
        let is_break = !paragraph.is_empty()
            && (item.start_ms.saturating_sub(previous_end) >= PARAGRAPH_GAP_MS
                || (paragraph.len() >= PARAGRAPH_CHAR && ends_sentence(&paragraph)));
        if is_break {
            out.push_str(&paragraph);
            out.push_str("\n\n");
            paragraph.clear();
        }
        if paragraph.is_empty() {
            paragraph.push_str(&stamp(item.start_ms));
        }
        paragraph.push(' ');
        paragraph.push_str(&item.text);
        previous_end = item.end_ms;
    }
    if !paragraph.is_empty() {
        out.push_str(&paragraph);
        out.push('\n');
    }
    out
}

#[derive(Serialize)]
struct Document<'a> {
    source: &'a str,
    language: &'a str,
    duration_ms: u64,
    cue: &'a [Cue],
    word: &'a [Word],
}

pub fn json(source: &str, language: &str, duration_ms: u64, cue: &[Cue], word: &[Word]) -> String {
    serde_json::to_string_pretty(&Document {
        source,
        language,
        duration_ms,
        cue,
        word,
    })
    .expect("plain data serialises")
}

#[cfg(test)]
mod test {
    use super::*;

    fn word(text: &str, start_ms: u64, end_ms: u64) -> Word {
        Word {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn a_title_does_not_end_the_cue() {
        let cue = cue_of(&[
            word("My", 0, 200),
            word("guest", 200, 500),
            word("is", 500, 600),
            word("Dr.", 600, 900),
            word("Okonkwo.", 900, 1_500),
        ]);
        assert_eq!(cue.len(), 1, "{cue:?}");
        assert!(ends_sentence("there.") && ends_sentence("Why?") && !ends_sentence("Dr."));
    }

    #[test]
    fn a_pause_starts_a_new_cue() {
        let cue = cue_of(&[
            word("Hello", 0, 400),
            word("there.", 450, 900),
            word("Next", 3_000, 3_400),
        ]);
        assert_eq!(cue.len(), 2);
        assert_eq!(cue[0].text, "Hello there.");
        assert_eq!(cue[1].start_ms, 3_000);
        // Lingered, but not into the next cue.
        assert_eq!(cue[0].end_ms, 1_300);
    }

    #[test]
    fn a_cue_never_outgrows_two_lines() {
        let many: Vec<Word> = (0..40)
            .map(|i| word("wordy", i * 200, i * 200 + 150))
            .collect();
        for item in cue_of(&many) {
            assert!(item.text.len() <= CUE_CHAR, "{item:?}");
            assert!(item.end_ms - item.start_ms <= CUE_MAX_MS + HOLD_MS);
        }
    }

    #[test]
    fn srt_is_numbered_with_comma_milliseconds() {
        let out = srt(&[Cue {
            start_ms: 3_723_456,
            end_ms: 3_724_000,
            text: "Hi.".into(),
        }]);
        assert_eq!(out, "1\n01:02:03,456 --> 01:02:04,000\nHi.\n\n");
    }

    #[test]
    fn vtt_has_its_header_and_dot_milliseconds() {
        let out = vtt(&[Cue {
            start_ms: 1_500,
            end_ms: 2_000,
            text: "Hi.".into(),
        }]);
        assert_eq!(out, "WEBVTT\n\n00:00:01.500 --> 00:00:02.000\nHi.\n\n");
    }

    #[test]
    fn long_text_wraps_into_two_balanced_lines() {
        let line = wrap("this sentence is long enough that it needs two lines here");
        let part: Vec<&str> = line.split('\n').collect();
        assert_eq!(part.len(), 2);
        assert!(part[0].len().abs_diff(part[1].len()) < 12, "{line}");
    }

    #[test]
    fn the_transcript_stamps_each_paragraph() {
        let out = txt(&[
            Cue {
                start_ms: 0,
                end_ms: 1_000,
                text: "One.".into(),
            },
            Cue {
                start_ms: 1_100,
                end_ms: 2_000,
                text: "Two.".into(),
            },
            Cue {
                start_ms: 65_000,
                end_ms: 66_000,
                text: "Later.".into(),
            },
        ]);
        assert_eq!(out, "[00:00] One. Two.\n\n[01:05] Later.\n");
    }
}
