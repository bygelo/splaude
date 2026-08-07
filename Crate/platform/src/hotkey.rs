//! Global push-to-talk binding.
//!
//! Push-to-talk is a *hold*, not a tap, so this module is only useful if it
//! reports the key going down **and** coming back up. `global-hotkey` does
//! carry both in [`global_hotkey::HotKeyState`], but it will only deliver them
//! if the host runs a real event loop on its main thread: on Windows the
//! manager is a hidden `HWND` and `WM_HOTKEY` reaches it through that thread's
//! message queue, on macOS the Carbon handler fires on the main run loop, and
//! macOS additionally refuses to let a library commandeer that thread. Those
//! two demands only reconcile one way — the caller owns the loop, this module
//! owns nothing but the registration. [`HotkeyListener::new`] must therefore be
//! called on the thread the app pumps; `splaude-app` builds its `tao` loop
//! there and constructs this immediately after.
//!
//! # Two `keyboard_types`, not one
//!
//! `splaude-core` speaks `keyboard-types` 0.8, `global-hotkey` 0.8 speaks
//! `keyboard-types` 0.7. Both are in the graph, so `splaude_core::Code` and
//! `global_hotkey::hotkey::Code` are *different types* that merely happen to
//! spell the same W3C vocabulary. [`to_registrable`] bridges them through that
//! shared spelling rather than pretending the versions unify.

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

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

// MARK: - Sink

/// Where an edge goes, plus the id that decides whether a press is ours.
struct Sink {
    /// Id of the chord currently registered, so a press for a binding we have
    /// already replaced is not mistaken for ours.
    current: u32,
    on_edge: Box<dyn Fn(HotkeyEdge) + Send + 'static>,
}

/// `global-hotkey` keeps its event handler in a `OnceCell` — set once per
/// process, never replaced or cleared. So the handler installed below is a
/// permanent trampoline and *this* is the part that changes: a listener writes
/// itself in on construction and takes itself out on drop, and after that no
/// edge reaches anyone.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Routes one crate-level event to the live listener, if there still is one.
///
/// Runs on whichever thread the backend emits from — the app's main thread on
/// Windows, since the trampoline is reached from inside the hidden window's
/// procedure while the `tao` loop is dispatching. The lock is held across
/// `on_edge`, so `on_edge` must not re-enter this module (`Mutex` is not
/// reentrant); the app satisfies that by only forwarding to its event loop.
fn deliver(event: GlobalHotKeyEvent) {
    let sink = SINK.lock().unwrap_or_else(|poison| poison.into_inner());
    let Some(sink) = sink.as_ref() else {
        return;
    };

    match event.state {
        // A press under an id we no longer hold is a leftover from the binding
        // we just replaced; starting a take on it would be wrong.
        HotKeyState::Pressed => {
            if event.id == sink.current {
                (sink.on_edge)(HotkeyEdge::Pressed);
            }
        }
        // Releases are forwarded whatever their id. Rebinding mid-hold changes
        // the id, and a swallowed release strands the take open with the
        // microphone still running — far worse than a spurious stop for a take
        // that was not running.
        HotKeyState::Released => (sink.on_edge)(HotkeyEdge::Released),
    }
}

fn with_sink<T>(edit: impl FnOnce(&mut Option<Sink>) -> T) -> T {
    let mut sink = SINK.lock().unwrap_or_else(|poison| poison.into_inner());
    edit(&mut sink)
}

// MARK: - Listener

/// Owns one live registration for as long as it exists.
///
/// Not `Send` on Windows, by way of the manager it holds: the `HWND` inside is
/// only addressable from the thread that created it. That is the type system
/// stating the rule the module header describes, so keep it.
pub struct HotkeyListener {
    manager: GlobalHotKeyManager,
    held: HotKey,
    binding: splaude_core::Hotkey,
}

impl HotkeyListener {
    /// Registers `binding` and starts reporting both of its edges.
    ///
    /// Must be called on the thread that pumps the app's event loop; see the
    /// module header for why no thread of our own can stand in for it.
    pub fn new(
        binding: splaude_core::Hotkey,
        on_edge: Box<dyn Fn(HotkeyEdge) + Send + 'static>,
    ) -> Result<Self> {
        // Before anything can emit. `global-hotkey` latches its handler slot on
        // the *first* event as well as on the first `set_event_handler`, so a
        // press that beat us here would pin the slot to `None` for the life of
        // the process and no edge would ever arrive again. The sink is still
        // empty, so the trampoline is a no-op until the registration lands.
        GlobalHotKeyEvent::set_event_handler(Some(deliver));

        let held = to_registrable(binding).map_err(|reason| anyhow!("{binding}: {reason}"))?;

        let manager = GlobalHotKeyManager::new()
            .map_err(|error| anyhow!("cannot bind {binding}: {error}"))?;

        // The common failure is not a bug but a fact about the machine: another
        // app already holds this chord. Say so, do not panic and do not go
        // quiet.
        manager
            .register(held)
            .map_err(|error| anyhow!("cannot register {binding}: {error}"))?;

        with_sink(|sink| {
            *sink = Some(Sink {
                current: held.id(),
                on_edge,
            })
        });

        Ok(Self {
            manager,
            held,
            binding,
        })
    }

    /// Moves the registration to `binding`, keeping the old one on refusal.
    ///
    /// Unregister-then-register, in that order: the id `global-hotkey`
    /// registers under is derived from the chord, so registering the new one
    /// first would leave two live registrations and report the same press
    /// twice.
    pub fn rebind(&mut self, binding: splaude_core::Hotkey) -> Result<()> {
        let next = to_registrable(binding).map_err(|reason| anyhow!("{binding}: {reason}"))?;
        if next == self.held {
            self.binding = binding;
            return Ok(());
        }

        if let Err(error) = self.manager.unregister(self.held) {
            splaude_core::diagnostic::log(
                "hotkey",
                format!("could not release {}: {error}", self.held),
            );
        }

        if let Err(error) = self.manager.register(next) {
            // A failed rebind must not leave the user with no hotkey at all.
            match self.manager.register(self.held) {
                Ok(()) => self.set_current(self.held.id()),
                Err(restore) => {
                    self.set_current(0);
                    splaude_core::diagnostic::log(
                        "hotkey",
                        format!(
                            "{next} refused and {} could not be restored: {restore}",
                            self.held
                        ),
                    );
                }
            }
            return Err(anyhow!("cannot register {binding}: {error}"));
        }

        self.held = next;
        self.binding = binding;
        self.set_current(next.id());
        Ok(())
    }

    /// The binding currently registered — after a failed [`Self::rebind`] this
    /// is still the old one, because a failed rebind restores it.
    pub fn binding(&self) -> splaude_core::Hotkey {
        self.binding
    }

    fn set_current(&self, id: u32) {
        with_sink(|sink| {
            if let Some(sink) = sink.as_mut() {
                sink.current = id;
            }
        });
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        // Order matters: silence the sink first, so an edge already queued in
        // the message loop cannot reach a callback whose owner is on its way
        // out.
        with_sink(|sink| *sink = None);

        // The one unregister that must not be skipped: leaving it registered
        // means the chord stays dead for every other app until the process
        // exits.
        if let Err(error) = self.manager.unregister(self.held) {
            splaude_core::diagnostic::log(
                "hotkey",
                format!("could not release {}: {error}", self.held),
            );
        }
    }
}

// MARK: - Test
//
// Mapping and sink routing only. Registering a real global hotkey needs a
// window server, so a test that did it would fail in CI for reasons that have
// nothing to do with this code.

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
        // The sink filters presses by this id, so equal chords must agree and
        // different chords must not.
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

    // The routing rules below are the whole reason push-to-talk survives a
    // rebind, and `deliver` is reachable without registering anything — the
    // sink is just a `static`. `SINK` is process-wide, so these run as one test
    // rather than racing each other across the harness's threads.
    #[test]
    fn the_sink_filters_presses_by_id_and_never_filters_releases() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::clone(&seen);
        let mine = parsed("Ctrl+KeyD").id();
        let stale = parsed("Alt+KeyD").id();

        with_sink(|sink| {
            *sink = Some(Sink {
                current: mine,
                on_edge: Box::new(move |edge| recording.lock().unwrap().push(edge)),
            })
        });

        deliver(GlobalHotKeyEvent {
            id: mine,
            state: HotKeyState::Pressed,
        });
        // The press for a chord we have already replaced must not start a take.
        deliver(GlobalHotKeyEvent {
            id: stale,
            state: HotKeyState::Pressed,
        });
        // The release for that same stale chord must still arrive, or a rebind
        // mid-hold would strand the take open.
        deliver(GlobalHotKeyEvent {
            id: stale,
            state: HotKeyState::Released,
        });

        assert_eq!(
            *seen.lock().unwrap(),
            [HotkeyEdge::Pressed, HotkeyEdge::Released]
        );

        // An empty sink is silent, which is what makes dropping a listener safe
        // while an edge is already in flight.
        let counted = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&counted);
        with_sink(|sink| {
            *sink = Some(Sink {
                current: mine,
                on_edge: Box::new(move |_| {
                    counting.fetch_add(1, Ordering::Relaxed);
                }),
            })
        });
        with_sink(|sink| *sink = None);
        deliver(GlobalHotKeyEvent {
            id: mine,
            state: HotKeyState::Released,
        });
        assert_eq!(counted.load(Ordering::Relaxed), 0);
    }
}
