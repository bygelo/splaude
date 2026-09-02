//! Fixing a word the recogniser always gets wrong.
//!
//! Keyterms bias what the recogniser *hears* — they raise the odds it produces
//! "Supabase" rather than "Super Base". They cannot help when it settles on a
//! spelling of its own and produces it reliably, which is the common case for
//! names it has never seen: the transcript is internally consistent and
//! consistently not what you said. Nothing between the transcript and the
//! keystrokes could change that, so the only remedy was to say the word
//! differently.
//!
//! This is a lookup table, and deliberately nothing more. It is not a command
//! grammar and not a formatter — those are refused in
//! `docs/roadmap-rejected.md` and for good reasons — it replaces literal text
//! with literal text.
//!
//! Two rules make it predictable, and both matter:
//!
//! **Longest key first.** With `"Claude" → "Claude"` and `"Claude Code" →
//! "Claude Code"` both present, matching the short one first would consume the
//! word and leave the longer rule permanently unreachable. Sorting by length
//! means the most specific rule that can apply, does.
//!
//! **The output is never re-scanned.** A rule like `"cat" → "cat cat"` is a
//! loop if replacement text is eligible for replacement. Here the cursor moves
//! past whatever was just emitted, so every rule fires at most once per
//! position and the pass always terminates.

use std::collections::BTreeMap;

/// The rules, from the text as heard to the text as wanted.
///
/// A `BTreeMap` rather than a `HashMap` because the settings file round-trips
/// through this: a map with a stable order produces the same JSON every save,
/// so writing a setting does not reshuffle the user's file underneath them.
pub type Rule = BTreeMap<String, String>;

/// Rewrites `text` by the rules, in one left-to-right pass.
///
/// Returns the input untouched when there is nothing to do, which is the case
/// on every take for every user who has not configured any.
#[must_use]
pub fn apply(text: &str, rule: &Rule) -> String {
    if rule.is_empty() || text.is_empty() {
        return text.to_string();
    }

    // Longest first — the module header says why. Built per call because the
    // rule set is a handful of entries and a take is a handful of frames; a
    // cached ordering would be state to keep in sync for no measurable gain.
    let mut ordered: Vec<(&String, &String)> =
        rule.iter().filter(|(from, _)| !from.is_empty()).collect();
    ordered.sort_by(|left, right| {
        right
            .0
            .len()
            .cmp(&left.0.len())
            // Ties broken by name so the result cannot depend on map iteration
            // order. Two rules of equal length that both match at the same
            // position is a contradiction the user wrote; answer it the same
            // way every time rather than differently per run.
            .then_with(|| left.0.cmp(right.0))
    });

    let mut out = String::with_capacity(text.len());
    let mut at = 0;

    while at < text.len() {
        let rest = &text[at..];

        match ordered
            .iter()
            .find(|(from, _)| rest.starts_with(from.as_str()))
        {
            Some((from, to)) => {
                out.push_str(to);
                at += from.len();
            }
            None => {
                // One character, not one byte: `at` must stay on a character
                // boundary or the next slice panics.
                let character = rest.chars().next().expect("rest is non-empty");
                out.push(character);
                at += character.len_utf8();
            }
        }
    }

    out
}

#[cfg(test)]
mod test {
    use super::*;

    fn rule(pair: &[(&str, &str)]) -> Rule {
        pair.iter()
            .map(|(from, to)| ((*from).to_string(), (*to).to_string()))
            .collect()
    }

    #[test]
    fn no_rule_leaves_the_text_alone() {
        assert_eq!(apply("hello there", &Rule::new()), "hello there");
    }

    #[test]
    fn a_rule_replaces_every_occurrence() {
        let rule = rule(&[("Cloud Code", "Claude Code")]);
        assert_eq!(
            apply("Cloud Code and Cloud Code again", &rule),
            "Claude Code and Claude Code again"
        );
    }

    /// The reason the rules are sorted. Without it the short rule eats the
    /// word and the long rule can never fire.
    #[test]
    fn the_longest_matching_rule_wins() {
        let rule = rule(&[("Cloud", "Claude"), ("Cloud Code", "Claude Code")]);
        assert_eq!(apply("Cloud Code", &rule), "Claude Code");
        // And the short rule still applies where the long one does not match.
        assert_eq!(apply("Cloud nine", &rule), "Claude nine");
    }

    /// The reason the output is never re-scanned. This rule is a loop under
    /// any implementation that reconsiders what it just wrote.
    #[test]
    fn a_rule_that_contains_its_own_key_terminates() {
        let rule = rule(&[("cat", "cat cat")]);
        assert_eq!(apply("one cat", &rule), "one cat cat");
    }

    #[test]
    fn a_rule_may_delete_text() {
        // Every occurrence, not the first: both "um " runs go.
        let rule = rule(&[("um ", "")]);
        assert_eq!(apply("um well um yes", &rule), "well yes");
    }

    #[test]
    fn matching_is_case_sensitive() {
        // Dictation is cased by the recogniser, and a rule that fired on any
        // casing would rewrite a word at the start of a sentence differently
        // from the same word inside one.
        let rule = rule(&[("swift", "Swift")]);
        assert_eq!(apply("swift and Swift", &rule), "Swift and Swift");
    }

    #[test]
    fn an_empty_key_is_ignored_rather_than_looping() {
        let rule = rule(&[("", "x"), ("a", "b")]);
        assert_eq!(apply("cat", &rule), "cbt");
    }

    #[test]
    fn multibyte_text_is_not_sliced_mid_character() {
        let rule = rule(&[("日本", "Japan")]);
        assert_eq!(apply("héllo 日本語 wörld", &rule), "héllo Japan語 wörld");
    }

    #[test]
    fn a_rule_whose_key_is_multibyte_matches() {
        let rule = rule(&[("café", "cafe")]);
        assert_eq!(apply("a café here", &rule), "a cafe here");
    }

    #[test]
    fn empty_text_is_returned_empty() {
        assert_eq!(apply("", &rule(&[("a", "b")])), "");
    }

    /// Applying twice must not differ from applying once for a rule whose
    /// output does not contain its input — which is what makes it safe to run
    /// on every interim frame of a live take.
    #[test]
    fn a_settled_rule_is_idempotent_across_frames() {
        let rule = rule(&[("Cloud Code", "Claude Code")]);
        let once = apply("Cloud Code", &rule);
        assert_eq!(apply(&once, &rule), once);
    }
}
