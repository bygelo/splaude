//! Does a synthetic keystroke actually arrive?
//!
//! This is the one subsystem in splaude with a field record, and it is a bad
//! one. Injection has broken in front of users three times — a remote desktop
//! turning a take into a run of `a`, the same window getting one letter per
//! character, and a paste chord that sent a bare `v` because the modifier was a
//! flag rather than an event — and the suite caught none of them. It could not
//! have: `injector.rs` has thirteen tests and every one is pure. Meanwhile the
//! socket, which has never broken in the field, has 687 lines of integration
//! test against a local server. The coverage was inverted with respect to the
//! risk, and this file is the correction.
//!
//! It owns a real window with a real `EDIT` control, injects into it through the
//! same [`Injector`] a take uses, and reads the characters back out. Nothing is
//! mocked: if the text in the control is not the text that was asked for, the
//! bug is the one a user would have hit.
//!
//! # Why this is `#[ignore]` by default
//!
//! It takes over the keyboard and the foreground window for a second or two.
//! Run it while typing and it will type into its own window instead of yours,
//! and the assertions will fail because your keystrokes landed in its `EDIT`.
//! That makes it hostile in a way an ordinary unit test is not, and unsafe to
//! have `cargo test` run by reflex. It also needs an interactive desktop, which
//! a CI runner may or may not give a test process.
//!
//! Run it deliberately, on a machine you are not using:
//!
//! ```text
//! cargo test -p splaude-platform --test injection -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is not optional. There is one foreground window and one
//! keyboard on a machine; two of these running at once would inject into each
//! other.
//!
//! **It needs an idle desktop, and says so when it does not get one.** Every
//! test here takes the foreground window and then checks it still has it at the
//! end. Run this while using the machine — or inside a remote-desktop session
//! somebody is working in — and a window will occasionally steal focus mid-test,
//! which fails with `something else took the foreground` rather than with an
//! assertion about text. That message is the harness being honest about its own
//! preconditions, not a bug in the injector; re-run it on a machine nobody is
//! touching.

#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use splaude_platform::Injector;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

/// How long to pump the message queue after injecting.
///
/// `SendInput` posts to the system's input queue; the characters do not exist
/// in the control until this thread has dispatched them. Generous because the
/// injector sleeps between chunks and the paste path deliberately waits on the
/// clipboard.
const SETTLE: Duration = Duration::from_millis(1_500);

/// Makes `window` the foreground window, working around the foreground lock.
///
/// `SetForegroundWindow` is not a request Windows always grants: a process that
/// does not already own the foreground is refused, which is a deliberate
/// anti-focus-stealing measure and exactly the position a test harness is in on
/// its second run. Attaching this thread's input queue to the current
/// foreground thread lifts the restriction for the length of the call, which is
/// the documented way through and the only one that is reliable.
///
/// Without this the suite passes once and then fails with "something else took
/// the foreground" forever after, which reads like an injection bug and is not
/// one.
fn take_foreground(window: HWND) {
    // Retried, because one attempt is not reliable. The window manager can be
    // mid-animation from the previous test's window closing, and the request is
    // then simply dropped — which shows up as the *next* test failing with
    // "something else took the foreground", a message that reads like an
    // injection bug and is not one. Asking again a moment later costs
    // milliseconds and removes a whole class of false alarm.
    for _ in 0..20 {
        // SAFETY: all of these take handles or thread ids by value. The
        // attachment is undone before the next iteration, including on the path
        // where the foreground thread is already this one.
        unsafe {
            if GetForegroundWindow() == window {
                return;
            }

            let foreground = GetForegroundWindow();
            let other = GetWindowThreadProcessId(foreground, None);
            let mine = GetCurrentThreadId();

            let attached =
                other != 0 && other != mine && AttachThreadInput(mine, other, true).as_bool();

            let _ = SetForegroundWindow(window);
            let _ = BringWindowToTop(window);

            if attached {
                let _ = AttachThreadInput(mine, other, false);
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until `window` actually holds the foreground, pumping while it waits.
///
/// Asking once is not enough and neither is asking twenty times in a tight
/// loop: activation is delivered as messages, so a window that never dispatches
/// any never finishes becoming active. This pumps between attempts, which is
/// the difference between a harness that passes and one that passes *usually*.
///
/// Answers whether it worked, so the caller can say so rather than failing
/// later with a message about injection.
fn settle_foreground(window: HWND, pump: impl Fn(Duration)) -> bool {
    for _ in 0..40 {
        take_foreground(window);
        pump(Duration::from_millis(50));

        // SAFETY: takes no arguments.
        if unsafe { GetForegroundWindow() } == window {
            return true;
        }
    }

    false
}

/// A window with a text field in it, owned by the test.
struct Target {
    window: HWND,
    field: HWND,
}

impl Target {
    fn open() -> Self {
        // SAFETY: a window class registered here, a window created from it, and
        // handles that live until `close`. Every argument is a constant or a
        // handle produced immediately above.
        unsafe {
            let instance = GetModuleHandleW(None).expect("module handle");

            // The default procedure, wrapped: `DefWindowProcW` is declared
            // `unsafe extern "system"` and the class wants a plain pointer to
            // one, which Rust will not coerce for us.
            unsafe extern "system" fn procedure(
                window: HWND,
                message: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }

            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: w!("splaude.injection.target"),
                hCursor: LoadCursorW(None, IDC_ARROW).expect("cursor"),
                ..Default::default()
            };
            // Zero means already registered, which happens on the second test
            // in the same process. Not an error.
            RegisterClassW(&class);

            let window = CreateWindowExW(
                // TOPMOST so nothing the developer has open covers it, and so
                // the keystrokes cannot land somewhere else because a chat
                // window popped up mid-test.
                WS_EX_TOPMOST,
                w!("splaude.injection.target"),
                w!("splaude injection target"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                420,
                160,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("target window");

            let field = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                10,
                10,
                380,
                28,
                Some(window),
                None,
                Some(instance.into()),
                None,
            )
            .expect("edit control");

            let _ = ShowWindow(window, SW_SHOW);

            let target = Self { window, field };
            // Before anything is typed, and pumping while it waits: without
            // this the first chunk goes wherever focus happened to be.
            assert!(
                settle_foreground(window, |how_long| target.pump(how_long)),
                "could not take the foreground — something on this desktop is holding it.                  This test needs an idle machine; see the module docs."
            );
            let _ = SetFocus(Some(field));
            target.pump(Duration::from_millis(100));
            target
        }
    }

    /// Dispatches queued messages for `how_long`.
    fn pump(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        let mut message = MSG::default();

        while Instant::now() < until {
            // SAFETY: a non-blocking peek on this thread's own queue.
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// What the field holds.
    fn text(&self) -> String {
        // SAFETY: the length is asked for first and the buffer sized from it.
        unsafe {
            let length = GetWindowTextLengthW(self.field);
            if length <= 0 {
                return String::new();
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let written = GetWindowTextW(self.field, &mut buffer);
            String::from_utf16_lossy(&buffer[..written as usize])
        }
    }

    fn clear(&self) {
        // SAFETY: a message to a control this test owns.
        unsafe {
            let _ = SetWindowTextW(self.field, w!(""));
            // Put the caret back at the end, so a subsequent injection appends
            // rather than overwriting a selection.
            SendMessageW(self.field, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        }
    }

    /// Whether the target still holds the foreground.
    ///
    /// Checked before asserting, because the most likely reason for a failure
    /// on a developer's machine is that something else stole focus — and
    /// reporting that as "injection is broken" would be a lie that costs
    /// somebody an afternoon.
    fn holds_focus(&self) -> bool {
        // SAFETY: takes no arguments.
        unsafe { GetForegroundWindow() == self.window }
    }

    fn close(self) {
        // SAFETY: destroying a window this test created and does not use again.
        unsafe {
            let _ = DestroyWindow(self.window);
        }
        // Drained, for the reason `Target::close` gives.
        self.pump(Duration::from_millis(150));
    }
}

/// A bare window that records what the keyboard actually delivered.
///
/// Separate from [`Target`] because the two answer different questions. A text
/// control tells you what a user would *see*; this tells you which keys were
/// pressed to put it there, and for the Return hazard only the second one can
/// tell the bug from the fix.
struct Watcher {
    window: HWND,
}

/// Characters delivered as `WM_CHAR`, and Return key-downs, since the last
/// [`Watcher::open`].
///
/// Statics because a window procedure is a bare function pointer with nowhere
/// to hang state, and the alternative — stashing a pointer in the window's user
/// data — is more unsafe code than a test needs. Safe here because this file
/// must run single-threaded anyway: there is one keyboard on a machine.
static SEEN: Mutex<String> = Mutex::new(String::new());
static RETURNS: AtomicUsize = AtomicUsize::new(0);

impl Watcher {
    fn open() -> Self {
        // SAFETY: as `Target::open` — a class, a window, and handles owned here.
        unsafe {
            let instance = GetModuleHandleW(None).expect("module handle");

            unsafe extern "system" fn procedure(
                window: HWND,
                message: u32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                const VK_RETURN: usize = 0x0D;

                match message {
                    WM_CHAR => {
                        if let Some(character) = char::from_u32(wparam.0 as u32) {
                            SEEN.lock().expect("seen").push(character);
                        }
                    }
                    // The thing this whole test exists to detect.
                    WM_KEYDOWN if wparam.0 == VK_RETURN => {
                        RETURNS.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }

                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }

            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: w!("splaude.injection.watcher"),
                hCursor: LoadCursorW(None, IDC_ARROW).expect("cursor"),
                ..Default::default()
            };
            RegisterClassW(&class);

            let window = CreateWindowExW(
                WS_EX_TOPMOST,
                w!("splaude.injection.watcher"),
                w!("splaude injection watcher"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                420,
                160,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("watcher window");

            let _ = ShowWindow(window, SW_SHOW);

            let watcher = Self { window };
            assert!(
                settle_foreground(window, |how_long| watcher.pump(how_long)),
                "could not take the foreground — something on this desktop is holding it.                  This test needs an idle machine; see the module docs."
            );
            let _ = SetFocus(Some(window));
            watcher.pump(Duration::from_millis(100));
            // Focus settling can itself deliver keystrokes, so only what
            // arrives after the injector runs is the subject.
            SEEN.lock().expect("seen").clear();
            RETURNS.store(0, Ordering::SeqCst);
            watcher
        }
    }

    fn pump(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        let mut message = MSG::default();

        while Instant::now() < until {
            // SAFETY: a non-blocking peek on this thread's own queue.
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn seen(&self) -> String {
        SEEN.lock().expect("seen").clone()
    }

    fn returns(&self) -> usize {
        RETURNS.load(Ordering::SeqCst)
    }

    fn holds_focus(&self) -> bool {
        // SAFETY: takes no arguments.
        unsafe { GetForegroundWindow() == self.window }
    }

    fn close(self) {
        // SAFETY: destroying a window this test created and does not use again.
        unsafe {
            let _ = DestroyWindow(self.window);
        }
        // Drained rather than abandoned. `DestroyWindow` queues the teardown,
        // and a window that is still half-alive is still competing for the
        // foreground — which is how the *next* test in the file ends up failing
        // with "something else took the foreground".
        self.pump(Duration::from_millis(150));
    }
}

/// Types `text` into a fresh target and hands back what arrived.
fn typed(text: &str) -> (String, bool) {
    let target = Target::open();
    target.clear();

    let mut injector = Injector::new().expect("synthetic input");
    injector.type_text(text, 1_200).expect("type");

    target.pump(SETTLE);

    let got = target.text();
    let focused = target.holds_focus();
    target.close();
    (got, focused)
}

#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn plain_text_arrives_verbatim() {
    let (got, focused) = typed("hello there");
    assert!(focused, "something else took the foreground mid-test");
    assert_eq!(got, "hello there");
}

/// The layout defence, checked rather than asserted in a comment. Every
/// character rides as a unicode payload precisely so it does not depend on
/// there being a key for it on the active layout.
#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn text_no_keyboard_layout_has_a_key_for_still_arrives() {
    let (got, focused) = typed("héllo wörld 日本語 — ✓");
    assert!(focused, "something else took the foreground mid-test");
    assert_eq!(got, "héllo wörld 日本語 — ✓");
}

/// Longer than one chunk, so the chunking and the per-chunk modifier clearing
/// are both exercised rather than skipped.
#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn a_sentence_longer_than_one_chunk_is_not_reordered() {
    let sentence = "The quick brown fox jumps over the lazy dog, twice, without dropping a word.";
    let (got, focused) = typed(sentence);
    assert!(focused, "something else took the foreground mid-test");
    assert_eq!(got, sentence);
}

/// **The regression that shipped.** A newline used to reach `enigo`, which
/// special-cases it into a real `VK_RETURN` click — so a transcript containing
/// one submitted whatever window the user was dictating into.
///
/// Asserted by counting the Return key-downs the window actually receives,
/// rather than by comparing text. That distinction is the whole value of this
/// test: a single-line `EDIT` silently ignores a Return, so a version of this
/// that only compared strings would pass with the bug present. What is wrong
/// with pressing Return is not what it puts on screen — it is that a chat box
/// sends, a search field searches, and a dialog closes.
#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn a_newline_does_not_press_return() {
    let watcher = Watcher::open();

    let mut injector = Injector::new().expect("synthetic input");
    injector.type_text("one\ntwo", 1_200).expect("type");

    watcher.pump(SETTLE);

    let seen = watcher.seen();
    let returns = watcher.returns();
    let focused = watcher.holds_focus();
    watcher.close();

    assert!(focused, "something else took the foreground mid-test");
    assert_eq!(
        returns, 0,
        "the injector pressed Return {returns} time(s) — a chat box would have sent"
    );
    // Both words still arrive: the fix is not "throw the rest of the take
    // away at the first newline".
    assert_eq!(seen, "one two", "saw {seen:?}");
}

/// **The regression that shipped, twice.** The paste chord is the delivery for
/// every remote-desktop and VM window, and it has been wrong in two different
/// ways: once carrying `Ctrl` as a flag with no key event behind it, so the far
/// end received a bare `v`, and once racing the clipboard so the *previous*
/// contents were pasted. A bare `v` in the field is precisely the failure this
/// asserts against.
///
/// # Why this one pumps while it pastes
///
/// Every other test here injects and then pumps. This one cannot, and the
/// reason is worth writing down because getting it wrong produced a very
/// convincing false alarm.
///
/// `Injector::paste` holds the clipboard for the length of the paste and then
/// puts the user's previous contents back — deliberately, and on a two-second
/// timer, because a redirected clipboard is slow. It is a *blocking* call. A
/// test that injects first and pumps afterwards therefore delivers `Ctrl+V` to
/// a window that will not process it until the restore has already happened, so
/// the field fills with whatever was on the clipboard *before* the take. Run
/// that inside a remote-desktop session whose clipboard is synced from another
/// machine and it fails with a stranger's text in it, which looks exactly like
/// the clipboard being stolen and is not.
///
/// A real application pumps its own queue continuously. This one has to as
/// well, or it is measuring a situation no user is ever in.
#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn the_paste_chord_pastes_rather_than_typing_a_v() {
    let target = Target::open();
    target.clear();

    let done = std::sync::Arc::new(AtomicUsize::new(0));
    let finished = std::sync::Arc::clone(&done);

    // The injector is built on the worker rather than moved to it: `Enigo` is
    // pinned to the thread that created it, which is why the app gives it a
    // thread of its own too.
    let worker = thread::spawn(move || {
        let mut injector = Injector::new().expect("synthetic input");
        injector
            .paste("pasted through the clipboard")
            .expect("paste");
        finished.fetch_add(1, Ordering::SeqCst);
    });

    // Pump while it works, the way the application being dictated into would.
    while done.load(Ordering::SeqCst) == 0 {
        target.pump(Duration::from_millis(50));
    }
    target.pump(Duration::from_millis(300));
    worker.join().expect("worker");

    let got = target.text();
    let focused = target.holds_focus();
    target.close();

    assert!(focused, "something else took the foreground mid-test");
    assert_ne!(
        got, "v",
        "the chord typed a bare v — the modifier never arrived"
    );
    assert_eq!(got, "pasted through the clipboard");
}

/// The take's own correction mechanism: live typing revises by backspacing.
/// Deleting one character must delete one character, not the word — which is
/// what `Ctrl+Backspace` does, and what a held modifier turns a backspace into.
#[test]
#[ignore = "takes the foreground window and the keyboard; run with --ignored --test-threads=1"]
fn a_backspace_removes_one_character_and_not_a_word() {
    let target = Target::open();
    target.clear();

    let mut injector = Injector::new().expect("synthetic input");
    injector.type_text("hello world", 1_200).expect("type");
    target.pump(Duration::from_millis(600));
    injector.backspace(1, 1_200).expect("backspace");
    target.pump(SETTLE);

    let got = target.text();
    let focused = target.holds_focus();
    target.close();

    assert!(focused, "something else took the foreground mid-test");
    assert_eq!(
        got, "hello worl",
        "a whole word went instead of a character"
    );
}
