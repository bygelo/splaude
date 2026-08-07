//! Synthetic keystrokes into whatever has focus.
//!
//! Two hazards shape everything here.
//!
//! **Layout.** Text must land verbatim on Dvorak, AZERTY, or a Vietnamese IME,
//! so nothing is ever translated to a keycode. `Keyboard::text` carries the
//! codepoint itself — `KEYEVENTF_UNICODE` on Windows, `set_string` on a
//! keycode-0 `CGEvent` on macOS, a keysym remap on X11 — which is the same
//! path `LiveTyper.swift` took and is layout-blind by construction.
//!
//! **Held modifiers.** Push-to-talk means the hotkey's modifier is physically
//! down while this runs, and the OS reads that hardware state when it
//! interprets our synthetic events. `Option+Delete` on macOS and
//! `Ctrl+Backspace` on Windows both eat a whole word instead of a character,
//! so a naive injector deletes the user's sentence one word per keystroke. See
//! `neutralize_modifier` for how each platform is defused.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

/// Characters per synthetic text event. Matches the Swift original's 16 UTF-16
/// units — small enough that Electron's input queue keeps up, large enough that
/// a sentence is a handful of events rather than a hundred.
const CHUNK_CHAR: usize = 16;

/// Modifiers that can promote a keystroke into a shortcut. Deliberately
/// includes Shift and Meta, not just the word-delete pair: Shift+Backspace and
/// Meta+Backspace are destructive in enough editors to be worth clearing too.
#[cfg(not(target_os = "macos"))]
const MODIFIER: [Key; 4] = [Key::Control, Key::Alt, Key::Shift, Key::Meta];

pub struct Injector {
    enigo: Enigo,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let setting = Settings {
            // macOS half of the held-modifier defence. True makes enigo build
            // its events from a `CGEventSourceStateID::Private` source and stamp
            // flags it tracks itself, so the hardware modifier state never
            // reaches our events — the exact semantic of `down.flags = []` in
            // LiveTyper.swift. Default is already true; pinned because the whole
            // correctness argument on macOS rests on it.
            independent_of_keyboard_state: true,
            ..Settings::default()
        };

        let enigo = Enigo::new(&setting).context("could not open a synthetic input connection")?;

        Ok(Self { enigo })
    }

    pub fn type_text(&mut self, text: &str, interval_micros: u32) -> Result<()> {
        // Text arrives from a speech model, so a NUL is implausible, but enigo
        // rejects the whole call on one and we would rather drop the byte than
        // the sentence.
        let clean = text.replace('\0', "");
        if clean.is_empty() {
            return Ok(());
        }

        let chunk = chunk_char(&clean, CHUNK_CHAR);
        let last = chunk.len().saturating_sub(1);

        for (index, part) in chunk.iter().enumerate() {
            self.neutralize_modifier()?;
            self.enigo
                .text(part)
                .context("could not enter text into the focused application")?;

            if index != last {
                sleep_micros(interval_micros);
            }
        }

        Ok(())
    }

    pub fn backspace(&mut self, count: usize, interval_micros: u32) -> Result<()> {
        for index in 0..count {
            self.neutralize_modifier()?;
            self.enigo
                .key(Key::Backspace, Direction::Click)
                .context("could not send backspace to the focused application")?;

            if index + 1 != count {
                sleep_micros(interval_micros);
            }
        }

        Ok(())
    }

    /// macOS needs nothing here: the private event source configured in `new`
    /// already detaches synthetic events from hardware modifier state, and
    /// injecting real key-up events would instead be seen by the hotkey layer
    /// as the user letting go of push-to-talk.
    #[cfg(target_os = "macos")]
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn neutralize_modifier(&mut self) -> Result<()> {
        Ok(())
    }

    /// Windows and X11 expose no per-event modifier mask — `SendInput` and
    /// `XTest` both interpret the keystroke against the global keyboard state.
    /// The only thing enigo (or the raw APIs underneath it) can express is an
    /// actual key-up, so we assert one for every modifier immediately before
    /// each synthetic event. The OS then reports the modifier as up while it
    /// resolves our keystroke, and `Ctrl+Backspace` cannot become a word
    /// delete.
    ///
    /// Repeated per event rather than once per burst on purpose: this mirrors
    /// the per-event flag clear in LiveTyper.swift, and a key-up for a key
    /// already up is free. Two consequences the caller should know about —
    /// a hotkey layer that detects release by polling key state may see the
    /// push-to-talk chord end early, and on Windows a lone Alt key-up can
    /// activate an app's menu bar. Both are strictly better than silently
    /// deleting the user's words a word at a time.
    #[cfg(not(target_os = "macos"))]
    fn neutralize_modifier(&mut self) -> Result<()> {
        for key in MODIFIER {
            self.enigo
                .key(key, Direction::Release)
                .context("could not clear a held modifier before typing")?;
        }

        Ok(())
    }
}

fn sleep_micros(micros: u32) {
    if micros > 0 {
        thread::sleep(Duration::from_micros(u64::from(micros)));
    }
}

/// Splits on char boundaries into runs of at most `len` chars. Grapheme
/// clusters may straddle a boundary; that is harmless because the pieces still
/// arrive in order and compose at the destination.
fn chunk_char(text: &str, len: usize) -> Vec<&str> {
    debug_assert!(len > 0);

    let mut part = Vec::new();
    let mut start = 0;
    let mut seen = 0;

    for (index, _) in text.char_indices() {
        if seen == len {
            part.push(&text[start..index]);
            start = index;
            seen = 0;
        }
        seen += 1;
    }

    if start < text.len() {
        part.push(&text[start..]);
    }

    part
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn empty_text_yields_no_chunk() {
        assert!(chunk_char("", 4).is_empty());
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_char("hi", 4), vec!["hi"]);
    }

    #[test]
    fn exact_multiple_does_not_emit_a_trailing_empty_chunk() {
        assert_eq!(chunk_char("abcdefgh", 4), vec!["abcd", "efgh"]);
    }

    #[test]
    fn remainder_becomes_the_last_chunk() {
        assert_eq!(chunk_char("abcdefghi", 4), vec!["abcd", "efgh", "i"]);
    }

    /// Chunking counts chars, not bytes — slicing mid-codepoint would panic.
    #[test]
    fn multibyte_text_splits_on_char_boundary() {
        let text = "héllo wörld";
        assert_eq!(chunk_char(text, 4), vec!["héll", "o wö", "rld"]);
        assert_eq!(chunk_char(text, 4).concat(), text);
    }

    #[test]
    fn astral_char_survives_chunking() {
        let text = "a🙂b🙂c";
        assert_eq!(chunk_char(text, 2), vec!["a🙂", "b🙂", "c"]);
        assert_eq!(chunk_char(text, 2).concat(), text);
    }

    #[test]
    fn chunk_reassembles_verbatim_at_every_width() {
        let text = "Ich hätte gern 日本語 — and emoji 👩‍💻 too.";
        for len in 1..=32 {
            assert_eq!(chunk_char(text, len).concat(), text, "width {len}");
            assert!(chunk_char(text, len)
                .iter()
                .all(|part| part.chars().count() <= len));
        }
    }
}
