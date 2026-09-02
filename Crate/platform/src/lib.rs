//! Per-OS integration.
//!
//! The macOS build reached straight for `AXUIElement`, `CGEvent`,
//! `RegisterEventHotKey` and `AVAudioEngine`. Each of those is a different API
//! on every platform, so each becomes a module here — and the app crate above
//! never learns which OS it is running on.
//!
//! Three of the five concerns need only one implementation, because a
//! cross-platform crate already covers them everywhere. Those are plain
//! structs; only the two that genuinely differ per OS carry `cfg` branches.
//!
//! | Concern      | Backend         | Windows   | macOS        | Linux              |
//! |--------------|-----------------|-----------|--------------|--------------------|
//! | Audio        | `cpal`          | WASAPI    | CoreAudio    | ALSA / PipeWire    |
//! | Hotkey       | `global-hotkey` | ✓         | ✓            | X11 only           |
//! | Injection    | `enigo`         | SendInput | `CGEvent`    | XTest / uinput     |
//! | Focus guard  | this crate      | real      | not yet      | not possible       |
//! | Autostart    | this crate      | registry  | LaunchAgent  | XDG autostart      |
//! | Return watch | this crate      | `WH_KEYBOARD_LL` | in the Swift app | none       |
//! | Start/stop tone | `cpal`       | WASAPI    | CoreAudio    | ALSA / PipeWire    |
//!
//! # Wayland
//!
//! Wayland deliberately denies both of this app's core gestures: a background
//! client cannot register a global hotkey, and it cannot synthesise input into
//! another client. That is a security design, not a gap to work around. The
//! Linux build therefore targets X11 (and XWayland) for full function, and on a
//! Wayland session it degrades honestly rather than failing silently — see
//! [`Capability`].

pub mod audio;
pub mod autostart;
pub mod control;
pub mod focus;
pub mod hotkey;
pub mod injector;
pub mod resample;
pub mod submit;
pub mod tone;

pub use audio::Capture;
pub use focus::FocusVerdict;
pub use hotkey::{HotkeyEdge, HotkeyListener};
pub use injector::Injector;
pub use tone::Tone;

/// What the current session actually permits, established at startup so the UI
/// can say so up front rather than after a take produces nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    pub global_hotkey: bool,
    pub inject_text: bool,
    /// Whether the focus guard can tell a text field from anything else.
    pub inspect_focus: bool,
    pub autostart: bool,
    /// Set when the session restricts the above by design, so the UI can say
    /// which and why instead of just reporting a dead hotkey.
    pub note: Option<String>,
}

impl Capability {
    /// Probes the running session.
    pub fn detect() -> Self {
        let session = Session::detect();

        Self {
            global_hotkey: session.can_reach_a_display(),
            inject_text: session.can_reach_a_display(),
            inspect_focus: focus::is_supported(),
            autostart: true,
            note: session.note(),
        }
    }

    /// Everything a take needs is available.
    pub fn can_dictate(&self) -> bool {
        self.global_hotkey && self.inject_text
    }
}

/// What kind of session this is, as far as the environment will admit.
///
/// Three answers rather than two, and the third is the one that was missing.
/// The probe used to ask only "is this Wayland?" and treat every other answer as
/// a working X11 session — so a machine with no display server at all, which is
/// what an SSH login is, reported that the hotkey and typing were both fine.
/// That is the worst shape a diagnostic can take: it is confidently wrong
/// exactly where someone is running it *because* nothing works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    /// A display server this build can drive.
    Graphical,
    /// Wayland, which refuses both gestures by design.
    Wayland,
    /// No display server named at all.
    Headless,
}

/// A variable that is set and not blank. `WAYLAND_DISPLAY=` is not a Wayland
/// session and `DISPLAY=` is not a display.
fn present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

impl Session {
    fn detect() -> Self {
        // Only Linux names its display server in the environment. macOS and
        // Windows have one whether or not anything says so, and a Windows
        // service with no desktop is not a case this probe can see from here.
        if !cfg!(target_os = "linux") {
            return Self::Graphical;
        }

        Self::of(
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("DISPLAY").ok().as_deref(),
        )
    }

    /// The rule itself, over the three variables that carry the answer.
    ///
    /// Separated from reading them so it can be tested: a test that set and
    /// unset real environment variables would be racing every other test in the
    /// binary, and this rule is exactly the kind that is wrong in the case
    /// nobody runs locally.
    fn of(
        session_type: Option<&str>,
        wayland_display: Option<&str>,
        display: Option<&str>,
    ) -> Self {
        let named_wayland = session_type.is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"));
        if named_wayland || present(wayland_display) {
            return Self::Wayland;
        }

        // `DISPLAY` is what every X11 client reads to find a server, so its
        // absence means there is nothing to connect to. Emptiness counts as
        // absence: `DISPLAY=` is set and useless, and a login shell can leave
        // it exactly that way.
        if present(display) {
            Self::Graphical
        } else {
            Self::Headless
        }
    }

    fn can_reach_a_display(self) -> bool {
        matches!(self, Self::Graphical)
    }

    fn note(self) -> Option<String> {
        match self {
            Self::Graphical => None,
            Self::Wayland => Some(
                "Wayland does not allow a background app to register a global \
                 hotkey or type into another window. Log into an X11 session \
                 for push-to-talk."
                    .to_string(),
            ),
            Self::Headless => Some(
                "No display server: DISPLAY is unset, so there is no session to \
                 register a hotkey with and no window to type into. This is the \
                 expected answer over SSH — run splaude on the machine you are \
                 sitting at."
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_session_that_cannot_hotkey_cannot_dictate() {
        let blocked = Capability {
            global_hotkey: false,
            inject_text: false,
            inspect_focus: false,
            autostart: true,
            note: Some("wayland".into()),
        };
        assert!(!blocked.can_dictate());
    }

    /// The case this probe used to get wrong, and the one it is most often run
    /// in: an SSH login, where nothing can work and the report used to say
    /// everything was fine.
    #[test]
    fn no_display_is_headless_rather_than_working_x11() {
        assert_eq!(Session::of(None, None, None), Session::Headless);
        assert!(!Session::of(None, None, None).can_reach_a_display());
        assert!(Session::of(None, None, None).note().is_some());
    }

    #[test]
    fn a_blank_display_is_not_a_display() {
        assert_eq!(Session::of(None, None, Some("")), Session::Headless);
        assert_eq!(Session::of(None, None, Some("   ")), Session::Headless);
    }

    #[test]
    fn an_x11_display_can_be_reached() {
        let session = Session::of(Some("x11"), None, Some(":0"));
        assert_eq!(session, Session::Graphical);
        assert!(session.can_reach_a_display());
        // Nothing is restricted, so there is nothing to say.
        assert!(session.note().is_none());
    }

    #[test]
    fn wayland_is_named_by_either_variable() {
        assert_eq!(Session::of(Some("wayland"), None, None), Session::Wayland);
        assert_eq!(Session::of(None, Some("wayland-0"), None), Session::Wayland);
        // Case is not guaranteed by anything, so it is not relied on.
        assert_eq!(Session::of(Some("Wayland"), None, None), Session::Wayland);
    }

    /// Wayland wins even with `DISPLAY` set, which is the normal state of a
    /// Wayland session running XWayland: the compositor is still the one
    /// refusing, and reporting X11 there would promise a hotkey that never
    /// fires.
    #[test]
    fn wayland_outranks_an_xwayland_display() {
        assert_eq!(
            Session::of(Some("wayland"), Some("wayland-0"), Some(":0")),
            Session::Wayland
        );
    }

    /// Each restricted session explains itself differently, because the
    /// remedies are different — log into X11, versus go and sit at the machine.
    #[test]
    fn every_restricted_session_says_something_of_its_own() {
        let wayland = Session::Wayland.note().unwrap();
        let headless = Session::Headless.note().unwrap();
        assert!(wayland.contains("Wayland"));
        assert!(headless.contains("DISPLAY"));
        assert_ne!(wayland, headless);
    }

    #[test]
    fn detect_reports_a_note_only_when_something_is_restricted() {
        let capability = Capability::detect();
        if capability.can_dictate() {
            assert!(capability.note.is_none());
        } else {
            // Never leave the user with a dead hotkey and no explanation.
            assert!(capability.note.is_some());
        }
    }
}
