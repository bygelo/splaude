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
//!
//! **The layout defence is also the remote-desktop wound.** `Keyboard::text`
//! puts the codepoint in the payload and leaves the keycode at 0. A native app
//! reads the payload; a remote-desktop or VM client re-encodes keyboard input
//! into scancodes for the wire, reads the *keycode*, and transmits whatever key
//! 0 is — `a` on macOS. Nothing about batching changes that: fifty characters
//! down the same path are fifty wrong characters. [`Injector::paste`] is the
//! other delivery, and it exists for exactly one reason — a real keycode
//! survives re-encoding, so the text rides the clipboard and only `Ctrl+V`
//! (`Cmd+V`) travels as keystrokes.

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

/// The modifier half of the paste chord. `Cmd` on macOS, `Ctrl` everywhere
/// else. Sent through `key`, not `text`, because that is the entire point of
/// the paste path: `key` emits a keycode a remote-desktop client can re-encode,
/// `text` emits a payload it throws away.
#[cfg(target_os = "macos")]
const PASTE_MODIFIER: Key = Key::Meta;
#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
const PASTE_MODIFIER: Key = Key::Control;

/// The `V` of the chord, as the platform's own virtual keycode — `kVK_ANSI_V`
/// on macOS, exactly the `9` in `TextInserter.swift`; `VK_V` on Windows.
///
/// Not `Key::Unicode('v')`, which would look more layout-aware and is in fact
/// the fragile choice here. Both platforms resolve that through the *active*
/// layout, and a layout with no `v` on it — Russian, Greek, Hebrew — has
/// nothing to resolve it to. enigo then falls back to entering it as unicode
/// text, which is the very payload-on-keycode-0 mechanism this path exists to
/// avoid, so the chord would arrive at a remote desktop as `Ctrl` plus
/// whatever key 0 is, and the paste would silently not happen. The letter keys
/// keep their ANSI keycodes under every layout, and paste is bound to the
/// keycode, so the constant is both simpler and more correct.
#[cfg(target_os = "macos")]
const PASTE_KEY: Key = Key::Other(9);
#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
const PASTE_KEY: Key = Key::Other(0x56);

/// How long the focused application gets to read the clipboard before the
/// previous contents go back. The same 0.35 s `TextInserter.swift` waits, and
/// for the same reason: the paste is asynchronous — the keystroke has only been
/// posted, not consumed — and restoring too early hands the app the *old*
/// clipboard to paste. The injector runs on a thread of its own whose only job
/// is delivery, and the take is over by the time this runs, so blocking here
/// costs nothing the user can perceive.
#[cfg(not(target_os = "linux"))]
const CLIPBOARD_SETTLE_MILLIS: u64 = 350;

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

    /// Delivers `text` by putting it on the clipboard and sending the paste
    /// chord as real keycodes.
    ///
    /// For applications that re-encode keystrokes by keycode, where
    /// [`Injector::type_text`] cannot work by construction. It is not the
    /// default delivery and should not become one: it costs the user their
    /// clipboard for a third of a second, and cannot always give it back.
    ///
    /// The clipboard is snapshotted and restored, but only as text — the
    /// `arboard` dependency here is built without `image-data`, so an image or
    /// a file list on the clipboard is not something this can round-trip. When
    /// the snapshot fails, nothing is put back and the take's text stays on the
    /// clipboard, which is the honest outcome: the old contents were already
    /// overwritten by the time we knew, and leaving the user's own words there
    /// beats leaving an empty clipboard.
    #[cfg(not(target_os = "linux"))]
    pub fn paste(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let mut clipboard =
            arboard::Clipboard::new().context("could not open the system clipboard")?;

        // `Err` here is "nothing we can put back" — an empty clipboard, or a
        // format this build cannot read. Not a reason to refuse the paste.
        let previous = clipboard.get_text().ok();

        clipboard
            .set_text(text)
            .context("could not put the take on the clipboard")?;

        let struck = self.paste_chord();

        // Restored whether or not the chord went out. A failed chord means the
        // caller is about to type the text instead, and it should not also
        // inherit a stomped clipboard.
        if let Some(previous) = previous {
            thread::sleep(Duration::from_millis(CLIPBOARD_SETTLE_MILLIS));
            if let Err(error) = clipboard.set_text(previous) {
                splaude_core::diagnostic::log(
                    "type",
                    format!("could not restore the clipboard after pasting: {error}"),
                );
            }
        }

        struck
    }

    /// Linux has no paste path, and this is a refusal rather than a gap.
    ///
    /// The only caller is the carve-out for keycode-translating applications,
    /// and that is driven by [`crate::focus::executable`], which answers `None`
    /// on Linux — there is no way to name the foreground process on Wayland at
    /// all, and only for cooperating clients on X11. So this is unreachable in
    /// practice, and pulling an X11 clipboard owner into a build that would
    /// never use it is a dependency for nothing. An error rather than a silent
    /// `Ok`: if the carve-out ever does reach here, the caller falls back to
    /// typing and the log says why, instead of the take vanishing.
    #[cfg(target_os = "linux")]
    #[allow(clippy::unused_self)]
    pub fn paste(&mut self, _text: &str) -> Result<()> {
        anyhow::bail!(
            "pasting is not implemented on Linux — no clipboard backend is \
             compiled in for this platform"
        )
    }

    /// Sends the platform's paste chord as real keycodes.
    ///
    /// The interaction with `neutralize_modifier` is the part that is easy to
    /// get silently wrong. It is called **once, before the chord opens**, and
    /// never inside it. Before, because push-to-talk means the user is
    /// physically holding a modifier: leave `Alt` down and `Ctrl+V` is read as
    /// `Ctrl+Alt+V`, which is somebody's macro and not a paste. Never inside,
    /// because the chord's own modifier is one of the keys `neutralize_modifier`
    /// releases — asserting a key-up for `Ctrl` between pressing it and striking
    /// `V` would hand the application a bare `v`, and the take would vanish
    /// leaving a single letter behind. That is why this does not reuse the
    /// per-event clearing `type_text` and `backspace` do.
    #[cfg(not(target_os = "linux"))]
    fn paste_chord(&mut self) -> Result<()> {
        self.neutralize_modifier()?;

        self.enigo
            .key(PASTE_MODIFIER, Direction::Press)
            .context("could not hold the paste modifier")?;

        let struck = self
            .enigo
            .key(PASTE_KEY, Direction::Click)
            .context("could not send the paste chord to the focused application");

        // Released unconditionally. A modifier left stuck down after a failed
        // keystroke turns the user's next real keypress into a shortcut, which
        // is a far worse state to leave the machine in than a lost paste.
        let released = self
            .enigo
            .key(PASTE_MODIFIER, Direction::Release)
            .context("could not release the paste modifier");

        struck.and(released)
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
