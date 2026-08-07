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
pub mod focus;
pub mod hotkey;
pub mod injector;
pub mod resample;

pub use audio::Capture;
pub use focus::FocusVerdict;
pub use hotkey::{HotkeyEdge, HotkeyListener};
pub use injector::Injector;

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
        let wayland = is_wayland();
        Self {
            global_hotkey: !wayland,
            inject_text: !wayland,
            inspect_focus: focus::is_supported(),
            autostart: true,
            note: wayland.then(|| {
                "Wayland does not allow a background app to register a global \
                 hotkey or type into another window. Log into an X11 session \
                 for push-to-talk."
                    .to_string()
            }),
        }
    }

    /// Everything a take needs is available.
    pub fn can_dictate(&self) -> bool {
        self.global_hotkey && self.inject_text
    }
}

fn is_wayland() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    std::env::var("XDG_SESSION_TYPE")
        .map(|kind| kind.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var("WAYLAND_DISPLAY").is_ok()
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
