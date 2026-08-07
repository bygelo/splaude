//! Reconciles what is on screen with what the recogniser now believes.
//!
//! The recogniser streams provisional text that mutates until an utterance ends
//! ("low testing" → "one, two, three"), so this keeps a copy of exactly what it
//! emitted, diffs each new target against it, and reports the minimum edit:
//! backspace only the characters that actually changed, then type the rest.
//!
//! The safety property is `locked`: text belonging to a *finished* utterance is
//! never backspaced over, so a revision can never chew backwards into words the
//! user typed themselves.
//!
//! This carries no OS dependency. It decides *what* to emit; the platform
//! injector decides *how*.

use crate::diagnostic;

/// Longest run of backspaces to accept before giving up and leaving the text
/// alone — a runaway diff must never machine-gun the delete key.
const MAX_REWRITE: usize = 240;

/// One reconciliation step: delete this many characters, then type this text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeAction {
    pub remove_count: usize,
    pub addition: String,
}

impl TypeAction {
    pub fn is_empty(&self) -> bool {
        self.remove_count == 0 && self.addition.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Typer {
    /// What this believes is on screen, past the insertion point.
    typed: String,
    /// Prefix length of `typed`, in characters, that is committed and must
    /// never be rewritten.
    locked: usize,
}

impl Typer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.typed.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.typed
    }

    pub fn reset(&mut self) {
        self.typed.clear();
        self.locked = 0;
    }

    /// Marks everything typed so far as final.
    pub fn lock(&mut self) {
        self.locked = self.typed.chars().count();
    }

    /// Diffs `target` against what is on screen and returns the edit to apply.
    ///
    /// Returns `None` when nothing needs to change, and also when the diff is
    /// too large to be worth trusting — in that case the internal copy is
    /// resynced to `target` without emitting anything, matching the Swift
    /// build's refusal rather than blindly deleting a paragraph.
    ///
    /// # Known divergence, inherited
    ///
    /// When the lock floor holds back a deletion the diff asked for, the
    /// bookkeeping below rebuilds this object's copy from `target`'s prefix
    /// rather than from the characters actually left on screen. The emitted
    /// edit is correct; the belief about the screen afterwards is not, and a
    /// later diff computed against it can be off. The Swift build does the
    /// same (`String(desired[0..<boundary]) + addition`), so this port keeps
    /// the behaviour rather than quietly changing what shipped — it needs a
    /// decision, not a silent fix.
    pub fn update(&mut self, target: &str) -> Option<TypeAction> {
        let current: Vec<char> = self.typed.chars().collect();
        let desired: Vec<char> = target.chars().collect();

        let mut shared = 0;
        while shared < current.len() && shared < desired.len() && current[shared] == desired[shared]
        {
            shared += 1;
        }

        // Never rewrite committed text, even if the diff wants to.
        let floor = self.locked.min(current.len()).min(desired.len());
        let boundary = shared.max(floor);

        let remove_count = current.len() - boundary;
        let addition: String = desired[boundary.min(desired.len())..].iter().collect();

        if remove_count == 0 && addition.is_empty() {
            return None;
        }

        if remove_count > MAX_REWRITE {
            diagnostic::log(
                "type",
                format!("refusing {remove_count}-character rewrite — resyncing instead"),
            );
            self.typed = target.to_string();
            return None;
        }

        let kept: String = desired[..boundary.min(desired.len())].iter().collect();
        self.typed = kept + &addition;

        Some(TypeAction {
            remove_count,
            addition,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn types_the_whole_string_when_empty() {
        let mut typer = Typer::new();
        let action = typer.update("hello").unwrap();
        assert_eq!(action.remove_count, 0);
        assert_eq!(action.addition, "hello");
        assert_eq!(typer.text(), "hello");
    }

    #[test]
    fn appends_without_rewriting_a_shared_prefix() {
        let mut typer = Typer::new();
        typer.update("hello").unwrap();
        let action = typer.update("hello there").unwrap();
        assert_eq!(action.remove_count, 0);
        assert_eq!(action.addition, " there");
    }

    #[test]
    fn backspaces_only_the_revised_tail() {
        let mut typer = Typer::new();
        typer.update("low testing").unwrap();
        let action = typer.update("low test").unwrap();
        assert_eq!(action.remove_count, 3);
        assert_eq!(action.addition, "");
        assert_eq!(typer.text(), "low test");
    }

    #[test]
    fn never_rewrites_locked_text() {
        let mut typer = Typer::new();
        typer.update("one two").unwrap();
        typer.lock();
        // A revision that wants to replace everything may only append.
        let action = typer.update("one two three").unwrap();
        assert_eq!(action.remove_count, 0);
        assert_eq!(action.addition, " three");
    }

    #[test]
    fn locked_prefix_survives_a_contradicting_target() {
        let mut typer = Typer::new();
        typer.update("committed").unwrap();
        typer.lock();

        // Every character is locked and the target is the same length, so the
        // lock floor eats the whole diff: nothing may be deleted and there is
        // no suffix left to add. The correct outcome is no edit at all —
        // emitting one could only mean chewing into committed words.
        assert_eq!(typer.update("different"), None);
        assert_eq!(typer.text(), "committed");
    }

    /// Pins a divergence inherited from the Swift build rather than endorsing
    /// it. See [`Typer::update`] — the emitted edit is right, the bookkeeping
    /// afterwards is not.
    #[test]
    fn a_longer_contradicting_target_may_only_append_past_the_lock() {
        let mut typer = Typer::new();
        typer.update("committed").unwrap();
        typer.lock();

        let action = typer.update("differently so").unwrap();
        // The edit itself is correct: nothing deleted, only the tail past the
        // locked prefix typed. The screen now reads "committedly so".
        assert_eq!(action.remove_count, 0);
        assert_eq!(action.addition, "ly so");

        // But the model records the *target's* prefix, not the text it left
        // alone — so it believes "differently so". Faithful to the original;
        // see the note on `update`.
        assert_eq!(typer.text(), "differently so");
    }

    #[test]
    fn refuses_a_runaway_rewrite_and_resyncs() {
        let mut typer = Typer::new();
        let long = "x".repeat(MAX_REWRITE + 10);
        typer.update(&long).unwrap();
        assert!(typer.update("y").is_none());
        // Resynced rather than emitted.
        assert_eq!(typer.text(), "y");
    }

    #[test]
    fn reports_nothing_when_the_target_is_unchanged() {
        let mut typer = Typer::new();
        typer.update("stable").unwrap();
        assert!(typer.update("stable").is_none());
    }

    #[test]
    fn counts_characters_not_bytes() {
        let mut typer = Typer::new();
        typer.update("café").unwrap();
        let action = typer.update("cafe").unwrap();
        // One character revised, not the two bytes "é" occupies.
        assert_eq!(action.remove_count, 1);
        assert_eq!(action.addition, "e");
    }
}
