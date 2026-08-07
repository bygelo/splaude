//! Global push-to-talk binding.
//!
//! Push-to-talk is a *hold*, not a tap, so this module is only useful if it
//! reports the key going down **and** coming back up. `global-hotkey` does
//! carry both in [`global_hotkey::HotKeyState`], but only if something keeps
//! draining its event channel — and on Windows, only if something keeps
//! pumping a win32 message queue on the thread that owns the manager. Both of
//! those loops live here, in threads this module owns, so the app crate above
//! never learns that a hidden `HWND` is involved.
//!
//! # Two `keyboard_types`, not one
//!
//! `splaude-core` speaks `keyboard-types` 0.8, `global-hotkey` 0.8 speaks
//! `keyboard-types` 0.7. Both are in the graph, so `splaude_core::Code` and
//! `global_hotkey::hotkey::Code` are *different types* that merely happen to
//! spell the same W3C vocabulary. [`to_registrable`] bridges them through that
//! shared spelling rather than pretending the versions unify.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// How long the forwarder blocks before re-checking whether it should still be
/// alive. It is the upper bound on how long `Drop` takes, and it costs one
/// wakeup every interval — 50ms is cheap and imperceptible on shutdown.
const FORWARDER_POLL: Duration = Duration::from_millis(50);

/// A push-to-talk binding is held, not tapped, so both edges matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEdge {
    Pressed,
    Released,
}

// MARK: - Mapping

/// Bridges a core binding to the chord `global-hotkey` can register.
///
/// The two crates disagree only about which `keyboard-types` they were built
/// against, so `Code` crosses as its W3C name — the one spelling both versions
/// generate from the same spec table. A name 0.7 does not know is a real
/// failure (that key cannot be registered), not something to paper over.
///
/// `META` is left as-is: `HotKey::new` rewrites it to `SUPER` itself, and doing
/// it twice here would just be a second opinion on the same question.
fn to_registrable(binding: splaude_core::Hotkey) -> Result<HotKey, String> {
    let mut modifier = Modifiers::empty();
    for (mine, theirs) in [
        (splaude_core::Modifiers::CONTROL, Modifiers::CONTROL),
        (splaude_core::Modifiers::ALT, Modifiers::ALT),
        (splaude_core::Modifiers::SHIFT, Modifiers::SHIFT),
        (splaude_core::Modifiers::META, Modifiers::META),
    ] {
        if binding.modifier.contains(mine) {
            modifier |= theirs;
        }
    }

    let name = binding.code.to_string();
    let code: Code = name
        .parse()
        .map_err(|_| format!("{name} is not a key this platform can bind"))?;

    Ok(HotKey::new(Some(modifier), code))
}

// MARK: - Listener

/// What the owner thread accepts. Everything that touches the manager goes
/// through here, because on Windows the manager is pinned to that one thread.
enum Command {
    Rebind(HotKey, Sender<Result<(), String>>),
    Shutdown,
}

pub struct HotkeyListener {
    command: Sender<Command>,
    /// Owns the `GlobalHotKeyManager` and, on Windows, its message pump.
    owner: Option<JoinHandle<()>>,
    /// Drains `GlobalHotKeyEvent::receiver()` into `on_edge`.
    forwarder: Option<JoinHandle<()>>,
    /// Cleared on drop; the forwarder blocks in bounded waits so it sees this.
    alive: Arc<AtomicBool>,
    /// Id of the chord currently registered, so a press for a binding we have
    /// already replaced is not mistaken for ours.
    current: Arc<AtomicU32>,
    /// Only used to wake a blocked `GetMessageW`. Zero elsewhere.
    #[cfg(windows)]
    owner_thread: u32,
    binding: splaude_core::Hotkey,
}

impl HotkeyListener {
    pub fn new(
        binding: splaude_core::Hotkey,
        on_edge: Box<dyn Fn(HotkeyEdge) + Send + 'static>,
    ) -> Result<Self> {
        let key = to_registrable(binding).map_err(|reason| anyhow!("{binding}: {reason}"))?;

        let alive = Arc::new(AtomicBool::new(true));
        let current = Arc::new(AtomicU32::new(key.id()));

        let (command, inbox) = mpsc::channel();
        let (ready, started) = mpsc::channel();

        let owned = Arc::clone(&current);
        let owner = std::thread::Builder::new()
            .name("splaude-hotkey".into())
            .spawn(move || serve(key, owned, ready, inbox))
            .map_err(|error| anyhow!("cannot start the hotkey thread for {binding}: {error}"))?;

        // Registration happens on that thread, so its verdict has to come back
        // before `new` can claim the binding is live.
        let outcome = started
            .recv()
            .map_err(|_| anyhow!("the hotkey thread died before registering {binding}"))?;

        let owner_thread = match outcome {
            Ok(thread) => thread,
            Err(reason) => {
                let _ = owner.join();
                return Err(anyhow!("cannot register {binding}: {reason}"));
            }
        };
        let _ = owner_thread;

        let watched = Arc::clone(&current);
        let watching = Arc::clone(&alive);
        let forwarder = std::thread::Builder::new()
            .name("splaude-hotkey-edge".into())
            .spawn(move || forward(on_edge, watched, watching))
            .map_err(|error| anyhow!("cannot start the hotkey forwarder for {binding}: {error}"))?;

        Ok(Self {
            command,
            owner: Some(owner),
            forwarder: Some(forwarder),
            alive,
            current,
            #[cfg(windows)]
            owner_thread,
            binding,
        })
    }

    pub fn rebind(&mut self, binding: splaude_core::Hotkey) -> Result<()> {
        let key = to_registrable(binding).map_err(|reason| anyhow!("{binding}: {reason}"))?;

        let (reply, answer) = mpsc::channel();
        self.command
            .send(Command::Rebind(key, reply))
            .map_err(|_| anyhow!("the hotkey thread is gone; cannot bind {binding}"))?;
        self.wake();

        answer
            .recv()
            .map_err(|_| anyhow!("the hotkey thread died while binding {binding}"))?
            .map_err(|reason| anyhow!("cannot register {binding}: {reason}"))?;

        self.binding = binding;
        Ok(())
    }

    /// The binding currently registered — after a failed [`Self::rebind`] this
    /// is still the old one, because a failed rebind restores it.
    pub fn binding(&self) -> splaude_core::Hotkey {
        self.binding
    }

    /// Nudges the owner thread out of its blocking wait.
    ///
    /// On Windows that wait is `GetMessageW`, which only returns for a message;
    /// without this, a rebind or a shutdown would sit in the queue until the
    /// user happened to press something. The posted message has a null `hwnd`,
    /// so dispatching it is a no-op — waking is the entire point.
    #[cfg(windows)]
    fn wake(&self) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

        unsafe {
            let _ = PostThreadMessageW(self.owner_thread, WM_APP, WPARAM(0), LPARAM(0));
        }
    }

    /// Elsewhere the owner thread blocks on the channel itself, so sending is
    /// already the wakeup.
    #[cfg(not(windows))]
    fn wake(&self) {}
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        // Order matters: clear `alive` first so the forwarder stops at its next
        // poll, then let the owner thread unregister and exit. Joining both is
        // what makes a rebind-by-replacement safe — a new listener cannot start
        // registering while the old chord is still live and still forwarding.
        self.alive.store(false, Ordering::Release);
        let _ = self.command.send(Command::Shutdown);
        self.wake();

        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
        if let Some(forwarder) = self.forwarder.take() {
            let _ = forwarder.join();
        }
        self.current.store(0, Ordering::Release);
    }
}

// MARK: - Owner thread

/// Owns the manager for its whole life and never lets it cross a thread.
///
/// `GlobalHotKeyManager` is not `Send` on Windows — it is a hidden `HWND`, and
/// win32 delivers `WM_HOTKEY` to the queue of the thread that created it. So
/// the manager is constructed here, every registration change is a [`Command`]
/// sent here, and the unregister on the way out happens here too.
fn serve(
    first: HotKey,
    current: Arc<AtomicU32>,
    ready: Sender<Result<u32, String>>,
    inbox: Receiver<Command>,
) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };

    // The common failure is not a bug but a fact about the machine: another app
    // already holds this chord. Say so, do not panic and do not go quiet.
    if let Err(error) = manager.register(first) {
        let _ = ready.send(Err(error.to_string()));
        return;
    }

    let mut held = first;
    current.store(held.id(), Ordering::Release);
    if ready.send(Ok(current_thread())).is_err() {
        let _ = manager.unregister(held);
        return;
    }

    while let Some(command) = next_command(&inbox) {
        match command {
            Command::Rebind(next, reply) => {
                let outcome = swap(&manager, &mut held, next, &current);
                let _ = reply.send(outcome);
            }
            Command::Shutdown => break,
        }
    }

    // The one unregister that must not be skipped: leaving it registered means
    // the chord stays dead for every other app until the process exits.
    if let Err(error) = manager.unregister(held) {
        splaude_core::diagnostic::log("hotkey", format!("could not release {held}: {error}"));
    }
}

/// Unregister-then-register, restoring the old chord if the new one is taken.
///
/// Doing it in this order matters: the id `global-hotkey` registers under is
/// derived from the chord, so registering the new one first would leave two
/// live registrations and the same press would be reported twice.
fn swap(
    manager: &GlobalHotKeyManager,
    held: &mut HotKey,
    next: HotKey,
    current: &AtomicU32,
) -> Result<(), String> {
    if *held == next {
        return Ok(());
    }

    if let Err(error) = manager.unregister(*held) {
        splaude_core::diagnostic::log("hotkey", format!("could not release {held}: {error}"));
    }

    if let Err(error) = manager.register(next) {
        // A failed rebind must not leave the user with no hotkey at all.
        match manager.register(*held) {
            Ok(()) => current.store(held.id(), Ordering::Release),
            Err(restore) => {
                current.store(0, Ordering::Release);
                splaude_core::diagnostic::log(
                    "hotkey",
                    format!("{next} refused and {held} could not be restored: {restore}"),
                );
            }
        }
        return Err(error.to_string());
    }

    *held = next;
    current.store(next.id(), Ordering::Release);
    Ok(())
}

/// Blocks until a command arrives, pumping win32 messages while it waits.
///
/// `global-hotkey` installs a window procedure and win32 posts `WM_HOTKEY` to
/// this thread's queue; without a `GetMessageW`/`DispatchMessageW` loop that
/// procedure is never called and *no* edge is ever emitted. The loop lives here
/// rather than in the app because this is the thread that owns the window.
#[cfg(windows)]
fn next_command(inbox: &Receiver<Command>) -> Option<Command> {
    use std::sync::mpsc::TryRecvError;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };

    loop {
        match inbox.try_recv() {
            Ok(command) => return Some(command),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }

        let mut message = MSG::default();
        // Blocking, not polling: a spin loop here would burn a core for the
        // life of the app. `HotkeyListener::wake` posts a message so a command
        // never waits on a keystroke to be noticed.
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if got.0 <= 0 {
            // 0 is WM_QUIT, -1 is an error; either way this queue is finished.
            return None;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// X11 runs its own event thread inside `global-hotkey`, and macOS delivers on
/// the application run loop, so there is no queue for us to pump — blocking on
/// the channel is the whole wait.
#[cfg(not(windows))]
fn next_command(inbox: &Receiver<Command>) -> Option<Command> {
    inbox.recv().ok()
}

#[cfg(windows)]
fn current_thread() -> u32 {
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

#[cfg(not(windows))]
fn current_thread() -> u32 {
    0
}

// MARK: - Forwarder thread

/// Drains the crate's global event channel into `on_edge`.
///
/// This has to be a second thread: on Windows the owner thread is parked in
/// `GetMessageW`, and the edge that `WM_HOTKEY` produces arrives on a
/// `crossbeam` channel, not the win32 queue. One consumer only — the channel is
/// process-wide, so a second listener would steal half the events.
fn forward(
    on_edge: Box<dyn Fn(HotkeyEdge) + Send + 'static>,
    current: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
) {
    let receiver = GlobalHotKeyEvent::receiver();

    while alive.load(Ordering::Acquire) {
        let event = match receiver.recv_timeout(FORWARDER_POLL) {
            Ok(event) => event,
            Err(error) if error.is_disconnected() => return,
            // A timeout is just the liveness check coming round.
            Err(_) => continue,
        };

        match event.state {
            // A press under an id we no longer hold is a leftover from the
            // binding we just replaced; starting a take on it would be wrong.
            HotKeyState::Pressed => {
                if event.id == current.load(Ordering::Acquire) {
                    on_edge(HotkeyEdge::Pressed);
                }
            }
            // Releases are forwarded whatever their id. Rebinding mid-hold
            // changes the id, and a swallowed release strands the take open
            // with the microphone still running — far worse than a spurious
            // stop for a take that was not running.
            HotKeyState::Released => on_edge(HotkeyEdge::Released),
        }
    }
}

// MARK: - Test
//
// Mapping only. Registering a real global hotkey needs a window server, so a
// test that did it would fail in CI for reasons that have nothing to do with
// this code.

#[cfg(test)]
mod test {
    use super::*;

    fn parsed(text: &str) -> HotKey {
        to_registrable(text.parse::<splaude_core::Hotkey>().unwrap()).unwrap()
    }

    #[test]
    fn maps_the_default_binding() {
        let key = to_registrable(splaude_core::Hotkey::default()).unwrap();
        assert_eq!(key.mods, Modifiers::ALT);
        assert_eq!(key.key, Code::Space);
    }

    #[test]
    fn carries_every_modifier_across_the_version_gap() {
        let key = parsed("Ctrl+Alt+Shift+KeyD");
        assert!(key.mods.contains(Modifiers::CONTROL));
        assert!(key.mods.contains(Modifiers::ALT));
        assert!(key.mods.contains(Modifiers::SHIFT));
        assert_eq!(key.key, Code::KeyD);
    }

    #[test]
    fn meta_becomes_super() {
        // Core stores the Cmd/Win key as META; global-hotkey registers SUPER.
        // If this ever stopped happening, Meta bindings would register bare.
        let key = parsed("Meta+KeyD");
        assert!(key.mods.contains(Modifiers::SUPER));
        assert!(!key.mods.contains(Modifiers::META));
    }

    #[test]
    fn a_bare_function_key_maps_without_a_modifier() {
        // The one unmodified binding core allows; it must survive the trip.
        let key = parsed("F13");
        assert!(key.mods.is_empty());
        assert_eq!(key.key, Code::F13);
    }

    #[test]
    fn the_id_distinguishes_bindings_and_is_stable() {
        // The forwarder filters presses by this id, so equal chords must agree
        // and different chords must not.
        assert_eq!(parsed("Ctrl+KeyD").id(), parsed("Ctrl+KeyD").id());
        assert_ne!(parsed("Ctrl+KeyD").id(), parsed("Alt+KeyD").id());
        assert_ne!(parsed("Ctrl+KeyD").id(), parsed("Ctrl+KeyE").id());
        assert_ne!(parsed("KeyD").id(), parsed("Ctrl+KeyD").id());
    }

    #[test]
    fn every_code_core_calls_safe_is_registrable() {
        // `Hotkey::is_safe` promises a bare F1..F20 is a legal binding. If 0.7
        // could not spell one of them, that promise would be a lie at runtime.
        for name in [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
            "F14", "F15", "F16", "F17", "F18", "F19", "F20",
        ] {
            let binding: splaude_core::Hotkey = name.parse().unwrap();
            assert!(binding.is_safe(), "{name} should be safe unmodified");
            assert!(to_registrable(binding).is_ok(), "{name} should map");
        }
    }

    #[test]
    fn a_code_the_backend_cannot_spell_is_an_error_not_a_panic() {
        // 0.8 knows codes 0.7 never had. Mapping one must fail with a message,
        // because the alternative is registering some other key silently.
        let exotic = splaude_core::Hotkey {
            modifier: splaude_core::Modifiers::CONTROL,
            code: splaude_core::Code::KeyboardBacklightToggle,
        };
        let reason = to_registrable(exotic).unwrap_err();
        assert!(reason.contains("KeyboardBacklightToggle"), "{reason}");
    }

    #[test]
    fn both_edges_exist_as_distinct_values() {
        assert_ne!(HotkeyEdge::Pressed, HotkeyEdge::Released);
    }
}
