//! Turning what the user knows about a recording into recogniser bias.
//!
//! The endpoint takes a list of key terms, not a prompt. People describe a
//! video in prose — "an interview with Dr. Okonkwo about CRISPR at Ateneo" —
//! or paste a list of names. Both become terms: a list is taken as written,
//! and prose contributes the words a general model is likely to get wrong —
//! names, acronyms, anything with odd casing or digits.

/// A word this common is never worth one of the limited term slots.
const COMMON: [&str; 40] = [
    "the",
    "and",
    "for",
    "with",
    "about",
    "this",
    "that",
    "from",
    "into",
    "our",
    "his",
    "her",
    "their",
    "they",
    "video",
    "audio",
    "podcast",
    "interview",
    "episode",
    "talk",
    "meeting",
    "is",
    "a",
    "an",
    "of",
    "in",
    "on",
    "at",
    "to",
    "by",
    "it",
    "we",
    "i",
    "you",
    "my",
    "mr",
    "mrs",
    "ms",
    "dr",
    "prof",
];

/// Terms from one piece of context, in the order they appear.
pub fn keyterm_of(context: &str) -> Vec<String> {
    let item: Vec<&str> = context
        .split(['\n', ',', ';'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect();

    // A list of short items is a list of terms.
    if item.len() > 1 && item.iter().all(|item| item.split_whitespace().count() <= 4) {
        return dedupe(item.into_iter().map(str::to_string));
    }

    dedupe(prose_term(context).into_iter())
}

fn is_notable(word: &str, is_sentence_start: bool) -> bool {
    let letter: Vec<char> = word.chars().filter(|c| c.is_alphabetic()).collect();
    if letter.is_empty() {
        return word.chars().any(|c| c.is_ascii_digit()) && word.len() > 1;
    }
    if COMMON.contains(&word.to_lowercase().as_str()) {
        return false;
    }
    let upper = letter.iter().filter(|c| c.is_uppercase()).count();
    let has_digit = word.chars().any(|c| c.is_ascii_digit());
    let is_acronym = upper >= 2;
    let is_odd_case = upper >= 1 && !letter[0].is_uppercase();
    let is_capitalised = letter[0].is_uppercase();
    is_acronym || is_odd_case || has_digit || (is_capitalised && !is_sentence_start)
}

/// Runs of notable words, so "Ateneo de Manila" style names stay together
/// where they are adjacent capitalised words.
fn prose_term(context: &str) -> Vec<String> {
    let mut term = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let mut is_sentence_start = true;

    for raw in context.split_whitespace() {
        let word =
            raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '\'');
        let word = word.trim_end_matches('.');
        // "Dr." ends with a full stop and does not end a sentence; the name
        // after a title has to stay eligible.
        let is_title = matches!(
            word.to_lowercase().as_str(),
            "dr" | "mr" | "mrs" | "ms" | "prof" | "st" | "sr" | "jr" | "fr"
        );
        let ends_sentence = raw.ends_with(['.', '!', '?', ':']) && !is_title;

        if !word.is_empty() && is_notable(word, is_sentence_start) {
            run.push(word.to_string());
        } else if !run.is_empty() {
            term.push(run.join(" "));
            run.clear();
        }
        if (ends_sentence || raw.ends_with([',', ';'])) && !run.is_empty() {
            term.push(run.join(" "));
            run.clear();
        }
        is_sentence_start = ends_sentence;
    }
    if !run.is_empty() {
        term.push(run.join(" "));
    }
    term
}

fn dedupe(term: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    term.filter(|item| seen.insert(item.to_lowercase()))
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_list_is_taken_as_written() {
        assert_eq!(
            keyterm_of("Supabase, Kubernetes\nAteneo de Manila"),
            ["Supabase", "Kubernetes", "Ateneo de Manila"]
        );
    }

    #[test]
    fn prose_contributes_names_acronyms_and_odd_casing() {
        let term = keyterm_of(
            "An interview with Dr. Ngozi Okonkwo about CRISPR and mRNA vaccines at the WHO in 2024",
        );
        assert!(term.contains(&"Ngozi Okonkwo".to_string()), "{term:?}");
        assert!(term.contains(&"CRISPR".to_string()), "{term:?}");
        assert!(term.contains(&"mRNA".to_string()), "{term:?}");
        assert!(term.contains(&"WHO".to_string()), "{term:?}");
        assert!(
            !term.iter().any(|t| t == "An" || t == "interview"),
            "{term:?}"
        );
    }

    #[test]
    fn repeats_count_once() {
        assert_eq!(
            keyterm_of("Splaude, splaude, Claude"),
            ["Splaude", "Claude"]
        );
    }
}
