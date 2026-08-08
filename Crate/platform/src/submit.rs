//! Watching for Return so that submitting ends the take.
//!
//! Submitting is a statement that you are done talking: in a chat box or a
//! search field the words after it would land somewhere the user cannot see.
//! `AppDelegate.swift` does this with `NSEvent.addGlobalMonitorForEvents`; this
//! is the same intent expressed with what Windows will actually give us.
//!
//! # Observe, never consume
//!
//! The whole behaviour is worthless if the keystroke does not also reach the
//! application. Swallowing Return would mean pressing it stopped the dictation
//! and sent nothing — the opposite of what someone hitting Return wants. So the
//! hook inspects and always chains: every path through [`backend::proc`] ends
//! at `CallNextHookEx`, including the ones that decide to stop the take.
//!
//! # Where it lives, and why it cannot live anywhere else
//!
//! `WH_KEYBOARD_LL` is a *thread* hook in a way the name does not advertise:
//! the OS calls it back by posting to the message queue of the thread that
//! installed it, so a hook installed on a thread with no pump is silently never
//! called. `splaude-app` already runs a `tao` event loop on the main thread for
//! `global-hotkey`, so that is the one thread in the process guaranteed to be
//! pumping, and both [`Watch::install`] and the drop that unhooks it must run
//! there. The type enforces it: [`Watch`] holds an `HHOOK`, which is a raw
//! pointer, so it is `!Send` and cannot be moved to another thread by accident.
//!
//! # It must not fire on splaude's own output
//!
//! See [`crate::injector::INJECTION_MARKER`] — matching on the virtual key
//! alone is not sufficient on Windows, because `enigo` posts a real `VK_RETURN`
//! for a `'\n'` in typed text. The marker in `dwExtraInfo` is what separates
//! our keystrokes from the user's.

use anyhow::Result;

pub use backend::Watch;

/// Windows virtual-key code for Return.
///
/// Numpad Enter reports this same code — only the extended-key flag in
/// `KBDLLHOOKSTRUCT::flags` separates the two — so one constant covers the pair
/// `AppDelegate.swift` has to name separately as `kVK_Return` and
/// `kVK_ANSI_KeypadEnter`.
#[cfg_attr(not(windows), allow(dead_code))]
const VK_RETURN: u32 = 0x0D;

/// Whether a key-down is the user submitting.
///
/// Pure over the two fields of `KBDLLHOOKSTRUCT` that decide it, so the rule
/// that keeps a take from stopping itself is testable with no hook, no desktop
/// session and no synthesised keystroke.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_submit(vk_code: u32, extra_info: usize) -> bool {
    vk_code == VK_RETURN && extra_info != crate::injector::INJECTION_MARKER
}

/// Whether the push-to-talk binding is itself the key this watcher matches on.
///
/// A take bound to `Ctrl+Enter` would otherwise stop itself: the low-level hook
/// sees the key-down before `WM_HOTKEY` reaches the loop, so starting a take
/// would immediately queue its own stop and the two would fight. Reporting the
/// collision lets the caller stand the watcher down for that binding rather
/// than ship a take that cannot be started.
///
/// `Code::Enter` is the main Return key and `Code::NumpadEnter` the other one,
/// which are exactly the two that map onto [`VK_RETURN`].
pub fn collides(binding: splaude_core::Hotkey) -> bool {
    matches!(
        binding.code,
        splaude_core::Code::Enter | splaude_core::Code::NumpadEnter
    )
}

/// Whether this platform has a Return watcher at all.
pub fn is_supported() -> bool {
    cfg!(windows)
}

/// Starts watching, on the platforms that can.
///
/// `on_submit` fires on the thread that installed the hook — the caller's own
/// event-loop thread — from inside the hook procedure, so it must do nothing
/// but hand the fact to whoever owns the take. It is not called for splaude's
/// own synthetic keystrokes.
///
/// `Ok(None)` is "this platform does not watch", which is macOS and Linux here
/// and is not a failure; `Err` is a Windows hook that could not be installed.
pub fn watch(on_submit: Box<dyn Fn() + Send + 'static>) -> Result<Option<Watch>> {
    backend::install(on_submit)
}

// MARK: - Windows

#[cfg(windows)]
mod backend {
    use std::sync::Mutex;

    use anyhow::{anyhow, Result};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    use super::is_submit;

    /// Where a matched Return goes, for as long as a [`Watch`] exists.
    ///
    /// The hook procedure is a bare `extern "system" fn` and can capture
    /// nothing, so the callback lives here — the same shape `crate::hotkey`
    /// uses for the same reason. `None` means no take is watching, and the
    /// procedure then does nothing but chain.
    static SINK: Mutex<Option<Box<dyn Fn() + Send + 'static>>> = Mutex::new(None);

    /// A live low-level keyboard hook. Dropping it uninstalls.
    ///
    /// `!Send` by way of the `HHOOK` it holds, which is the type system stating
    /// the module header's rule: the hook belongs to the thread that installed
    /// it, and `UnhookWindowsHookEx` has to run there too.
    pub struct Watch {
        hook: HHOOK,
    }

    pub fn install(on_submit: Box<dyn Fn() + Send + 'static>) -> Result<Option<Watch>> {
        // Before the hook exists, so a keystroke that arrives between the two
        // finds a callback rather than an empty sink.
        with_sink(|sink| *sink = Some(on_submit));

        // SAFETY: `proc` is a `'static` function with the signature the OS
        // documents for `WH_KEYBOARD_LL`. A null module handle is what a hook
        // procedure inside the current executable wants — the module argument
        // is only meaningful for a proc in a separate DLL — and thread id 0
        // asks for the global hook, which is the only kind `WH_KEYBOARD_LL`
        // supports.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), None, 0) };

        match hook {
            Ok(hook) => Ok(Some(Watch { hook })),
            Err(error) => {
                // Leaving a callback behind a hook that was never installed
                // would strand it for the life of the process.
                with_sink(|sink| *sink = None);
                Err(anyhow!("could not watch for Return: {error}"))
            }
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            // Silence the sink first, so a keystroke already in the queue
            // cannot reach a callback whose owner is on its way out — the same
            // ordering `crate::hotkey`'s listener drops in.
            with_sink(|sink| *sink = None);

            // SAFETY: `self.hook` came from `SetWindowsHookExW` above, has not
            // been unhooked before (nothing else touches it, and `Drop` runs
            // once), and this is the thread that installed it.
            if let Err(error) = unsafe { UnhookWindowsHookEx(self.hook) } {
                splaude_core::diagnostic::log(
                    "submit",
                    format!("could not remove the Return watcher: {error}"),
                );
            }
        }
    }

    /// The hook procedure.
    ///
    /// Runs inside the OS's input path for **every** keystroke on the desktop,
    /// so it stays short: two integer comparisons and, at most, one non-blocking
    /// send. A hook that takes too long is silently removed by Windows, and one
    /// that blocks stalls typing everywhere.
    ///
    /// # Safety
    ///
    /// Called by the OS with the `WH_KEYBOARD_LL` contract: a negative `code`
    /// must be passed straight on, and for `HC_ACTION` the `lparam` is a
    /// pointer to a `KBDLLHOOKSTRUCT` that is live for the duration of the call.
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let message = wparam.0 as u32;
            if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
                // SAFETY: the contract above — for `HC_ACTION` this pointer is
                // a live `KBDLLHOOKSTRUCT` owned by the caller, and the borrow
                // ends before this function returns.
                let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
                if is_submit(event.vkCode, event.dwExtraInfo) {
                    notify();
                }
            }
        }

        // Unconditional, on every path including the one that just stopped a
        // take. This is what makes the watcher an observer: Return still
        // reaches the application and still sends the message.
        //
        // SAFETY: forwards the arguments the OS handed us, unmodified. `None`
        // for the handle is the documented way to say "the current hook chain".
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    fn notify() {
        let sink = SINK.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(on_submit) = sink.as_ref() {
            on_submit();
        }
    }

    fn with_sink<T>(edit: impl FnOnce(&mut Option<Box<dyn Fn() + Send + 'static>>) -> T) -> T {
        let mut sink = SINK.lock().unwrap_or_else(|poison| poison.into_inner());
        edit(&mut sink)
    }
}

// MARK: - Everywhere else

/// macOS and Linux have no watcher here, and that is a decision rather than a
/// gap.
///
/// macOS already ships one — `AppDelegate.swift`'s global `NSEvent` monitor —
/// in the Swift app that is what macOS users run today, and reproducing it
/// would mean AppKit FFI this crate does not carry. On X11 an equivalent means
/// grabbing the keyboard or the XInput2 raw event stream, neither of which
/// observes without cost, and Wayland refuses the introspection outright for
/// the same reason it refuses everything else this app wants.
///
/// [`Watch`] is uninhabited here rather than a unit struct, so the `Option` the
/// caller stores is statically `None` and nothing about the take path changes
/// on those platforms.
#[cfg(not(windows))]
mod backend {
    use anyhow::Result;

    pub enum Watch {}

    pub fn install(_on_submit: Box<dyn Fn() + Send + 'static>) -> Result<Option<Watch>> {
        Ok(None)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Nothing here installs a hook or synthesises a keystroke: the matching
    // rule is pure precisely so it can be checked in CI, and the hook itself
    // needs a desktop session and a message pump.

    /// Not splaude's, so it is a real press.
    const USER: usize = 0;

    #[test]
    fn return_from_the_user_ends_the_take() {
        assert!(is_submit(VK_RETURN, USER));
    }

    #[test]
    fn our_own_return_never_ends_the_take() {
        // The hazard this exists for: `enigo`'s Windows `text()` posts a real
        // VK_RETURN for a newline in a transcript, which is indistinguishable
        // from a keypress by virtual key alone.
        assert!(!is_submit(VK_RETURN, crate::injector::INJECTION_MARKER));
    }

    #[test]
    fn nothing_else_we_ever_type_looks_like_return() {
        // Every virtual key splaude can put on the wire, marker aside. The
        // unicode payload path (`VIRTUAL_KEY(0)`), the packet code the OS
        // substitutes for it, backspace, and the paste chord.
        for vk in [
            0x00, // VIRTUAL_KEY(0), what `KEYEVENTF_UNICODE` demands
            0xE7, // VK_PACKET, what the hook sees for an injected codepoint
            0x08, // VK_BACK, the live-typing diff's corrections
            0x11, // VK_CONTROL, half the paste chord
            0x56, // VK_V, the other half
            0x09, // VK_TAB, which `text()` special-cases like it does newline
        ] {
            assert!(!is_submit(vk, USER), "virtual key {vk:#04x}");
        }
    }

    #[test]
    fn an_ordinary_key_is_not_a_submission() {
        for vk in [0x41, 0x20, 0x1B, 0x0C, 0x0E] {
            assert!(!is_submit(vk, USER), "virtual key {vk:#04x}");
        }
    }

    #[test]
    fn a_return_binding_collides_and_an_ordinary_one_does_not() {
        // Both spellings of the key, because both map onto VK_RETURN and only
        // the extended-key flag tells them apart at the hook.
        assert!(collides("Ctrl+Enter".parse().unwrap()));
        assert!(collides("Ctrl+NumpadEnter".parse().unwrap()));

        // The defaults above all: neither platform's push-to-talk key may
        // stand the watcher down.
        assert!(!collides(splaude_core::Hotkey::default()));
        for text in ["F9", "Alt+Slash", "Alt+Space", "Ctrl+Shift+KeyD"] {
            assert!(!collides(text.parse().unwrap()), "{text}");
        }
    }

    #[test]
    fn support_is_claimed_only_where_it_is_implemented() {
        assert_eq!(is_supported(), cfg!(windows));
    }

    #[test]
    fn a_platform_without_a_watcher_declines_rather_than_fails() {
        // On Windows this would install a real hook, which needs a session;
        // elsewhere the contract is that asking is harmless and answers `None`.
        if !is_supported() {
            assert!(watch(Box::new(|| {})).unwrap().is_none());
        }
    }
}
