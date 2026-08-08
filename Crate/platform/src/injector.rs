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
//! **The defence's own wound: the binding's modifier.** Releasing a modifier on
//! Windows is not a local act. `RegisterHotKey` decides whether a physical
//! keypress is the registered chord — and therefore whether the focused window
//! ever sees it — by reading that same global modifier state. Assert a key-up
//! for `Alt` while the user holds `Alt+Space` and the chord stops matching, so
//! their still-held `Space` stops being swallowed and auto-repeats into the
//! focused window as ordinary spaces, interleaved with the take. That is what
//! [`Injector::set_binding`] is for: the modifier that is part of the *live*
//! binding is left alone, every other one is still cleared, because a stray
//! held `Ctrl` is the original hazard and has not gone away. macOS needs none
//! of this — the private event source means no modifier is ever touched.
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

/// The `dwExtraInfo` word stamped on every synthetic event this app posts on
/// Windows — `'SPLD'`, the same four bytes `Hotkey.swift` signs its Carbon
/// registration with.
///
/// It exists for [`crate::submit`], which watches for Return and must never
/// fire on splaude's own output. Matching on the virtual key alone is *not*
/// enough here, and that is a fact about this exact `enigo` version rather than
/// a hypothetical: `Keyboard::text` on Windows special-cases `'\n'` and posts a
/// real `VK_RETURN` click for it before the unicode payload, so a transcript
/// containing a newline would type a keystroke indistinguishable from the user
/// pressing Return. macOS has no such hole — `LiveTyper` posts everything on
/// virtual key 0 — which is why the Swift build can match on keycode alone and
/// this one cannot.
///
/// `enigo`'s own default is [`enigo::EVENT_MARKER`], the number 100, which any
/// other `enigo` application on the machine would also stamp; a value of our own
/// means the watcher ignores *our* keystrokes and nobody else's.
///
/// Deliberately not `LLKHF_INJECTED`: that flag is set for every synthetic
/// event on the desktop, so keying off it would also ignore the Return of
/// someone driving their keyboard through PowerToys, AutoHotkey or an
/// on-screen keyboard — a real user submitting, by any reasonable reading.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const INJECTION_MARKER: usize = 0x5350_4C44;

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

    /// The modifier `neutralize_modifier` must leave alone, because the live
    /// push-to-talk binding is made of it. Empty until [`Injector::set_binding`]
    /// says otherwise, which is the safe default: an empty exclusion is exactly
    /// the unconditional clearing this module did before, so a caller that never
    /// sets a binding gets the old behaviour rather than a stuck modifier.
    #[cfg(not(target_os = "macos"))]
    binding_modifier: Vec<Key>,
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
            // Read only on Windows — the field is in the shared `Settings` on
            // every platform, and setting it where nothing reads it is cheaper
            // than a `cfg` around one line. See [`INJECTION_MARKER`].
            windows_dw_extra_info: Some(INJECTION_MARKER),
            ..Settings::default()
        };

        let enigo = Enigo::new(&setting).context("could not open a synthetic input connection")?;

        Ok(Self {
            enigo,
            #[cfg(not(target_os = "macos"))]
            binding_modifier: Vec::new(),
        })
    }

    /// Tells the injector which chord push-to-talk is currently bound to.
    ///
    /// A setter rather than a constructor argument because the binding is not a
    /// fact about the injector's lifetime: [`crate::HotkeyListener::rebind`] can
    /// move it while the process runs, and an injector built once at startup
    /// would otherwise go on protecting a chord the user has abandoned — sparing
    /// a modifier nobody is holding, and clearing the one they now are. Whoever
    /// owns the listener owns this call, and must repeat it after every
    /// successful rebind.
    ///
    /// A no-op on macOS: nothing is released there, so there is nothing to
    /// exclude.
    #[cfg(not(target_os = "macos"))]
    pub fn set_binding(&mut self, binding: splaude_core::Hotkey) {
        self.binding_modifier = binding_modifier(binding);
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::unused_self)]
    pub fn set_binding(&mut self, _binding: splaude_core::Hotkey) {}

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
    ///
    /// The binding's own modifier is skipped; the module header says why. The
    /// cost of skipping it is real and worth naming: with `Alt` still down, an
    /// app that treats `Alt+Backspace` as undo will do that instead of deleting
    /// a character, and Windows may route our synthetic text as `WM_SYSCHAR`.
    /// The alternative is the leak, which corrupts every take on a printable
    /// binding rather than misbehaving in some apps, so this is the side to err
    /// on — but a binding whose modifier is `Alt` is the least comfortable
    /// shape, and a bare function key needs no exclusion at all.
    #[cfg(not(target_os = "macos"))]
    fn neutralize_modifier(&mut self) -> Result<()> {
        for key in MODIFIER {
            if self.binding_modifier.contains(&key) {
                continue;
            }

            self.enigo
                .key(key, Direction::Release)
                .context("could not clear a held modifier before typing")?;
        }

        Ok(())
    }
}

/// The modifier keys a binding is made of, as the keys the injector would
/// otherwise release.
///
/// Pure, and deliberately so: it is the whole correctness argument of the
/// exclusion, and testing it must not require a window server or a held key.
///
/// Only the modifier crosses over, never `binding.code`. A synthetic key-up for
/// the binding's *key* would not stop the leak it looks like it addresses —
/// auto-repeat is driven by the physical key being down, and the driver keeps
/// posting fresh key-downs regardless of what key-ups we inject in between, so
/// it would suppress nothing. It would also be actively harmful: a key-up for
/// the bound key is indistinguishable from the user letting go of push-to-talk,
/// which is how a take ends. The exclusion above is what stops the leak; there
/// is nothing left for the key itself to do.
///
/// The output is empty for an unmodified binding — a bare `F13` — which is the
/// correct answer twice over. Nothing we release is holding `F13` down, so it
/// cannot leak in the first place, and clearing all four modifiers is exactly
/// the behaviour that has always been right when no modifier belongs to the
/// chord.
#[cfg(not(target_os = "macos"))]
fn binding_modifier(binding: splaude_core::Hotkey) -> Vec<Key> {
    // `Key::Meta` is enigo's name for the key `splaude_core` calls META and
    // Windows calls Win — same physical key, three vocabularies.
    [
        (splaude_core::Modifiers::CONTROL, Key::Control),
        (splaude_core::Modifiers::ALT, Key::Alt),
        (splaude_core::Modifiers::SHIFT, Key::Shift),
        (splaude_core::Modifiers::META, Key::Meta),
    ]
    .into_iter()
    .filter(|(flag, _)| binding.modifier.contains(*flag))
    .map(|(_, key)| key)
    .collect()
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

    /// Nothing here presses a key. The exclusion is a pure mapping precisely so
    /// it can be checked without a window server or a physically held chord.
    #[cfg(not(target_os = "macos"))]
    mod exclusion {
        use super::*;

        fn spared(text: &str) -> Vec<Key> {
            binding_modifier(text.parse::<splaude_core::Hotkey>().unwrap())
        }

        #[test]
        fn each_modifier_maps_to_its_enigo_key() {
            assert_eq!(spared("Ctrl+KeyD"), vec![Key::Control]);
            assert_eq!(spared("Alt+KeyD"), vec![Key::Alt]);
            assert_eq!(spared("Shift+KeyD"), vec![Key::Shift]);
            assert_eq!(spared("Meta+KeyD"), vec![Key::Meta]);
        }

        /// On Windows the default is a bare function key, and sparing *nothing*
        /// is the whole point: with no modifier to hold there is none to inherit
        /// into a backspace, and none to release that was suppressing the bound
        /// key. Elsewhere the default is `Alt+Slash`, where Alt must survive the
        /// clearing or the chord stops matching and the held Slash leaks into
        /// the take. Both directions are the same rule seen from two platforms.
        #[test]
        fn the_default_binding_spares_only_what_it_holds() {
            let spared = binding_modifier(splaude_core::Hotkey::default());

            if cfg!(target_os = "windows") {
                assert!(
                    spared.is_empty(),
                    "the Windows default must carry no modifier, spared {spared:?}"
                );
            } else {
                assert_eq!(spared, vec![Key::Alt]);
            }
        }

        #[test]
        fn a_printable_key_contributes_nothing_of_its_own() {
            // Only the modifier crosses over. Slash and Space are both printable
            // and both bound to Alt, so they must map identically — the key
            // itself is never released.
            assert_eq!(spared("Alt+Slash"), spared("Alt+Space"));
            assert_eq!(spared("Alt+Slash"), vec![Key::Alt]);
        }

        #[test]
        fn a_bare_function_key_spares_nothing() {
            // Nothing we release is holding F13 down, so every modifier is
            // still cleared — the pre-existing behaviour, unchanged.
            assert!(spared("F13").is_empty());
        }

        #[test]
        fn a_multi_modifier_binding_spares_all_of_them() {
            let spared = spared("Ctrl+Alt+Shift+Meta+KeyA");
            for key in MODIFIER {
                assert!(spared.contains(&key), "{key:?} should be spared");
            }
        }

        /// Whatever is spared has to be drawn from the set the injector would
        /// otherwise release, or the exclusion cannot exclude anything.
        #[test]
        fn every_spared_key_is_one_the_injector_releases() {
            for text in ["Ctrl+KeyD", "Alt+Slash", "Shift+F5", "Meta+KeyD", "F13"] {
                for key in spared(text) {
                    assert!(MODIFIER.contains(&key), "{key:?} from {text}");
                }
            }
        }
    }

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
