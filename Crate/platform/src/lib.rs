//! Per-OS integration behind one trait set.
//!
//! The macOS build reached straight for `AXUIElement`, `CGEvent`,
//! `RegisterEventHotKey` and `AVAudioEngine`. Each of those is a different API
//! on every platform, so each becomes a trait here — and the app crate above
//! never learns which OS it is running on.
//!
//! Three of the six concerns turned out to need only one implementation, because
//! a cross-platform crate already covers them everywhere:
//!
//! | Concern      | Backend        | Windows | macOS | Linux                  |
//! |--------------|----------------|---------|-------|------------------------|
//! | Audio        | `cpal`         | WASAPI  | CoreAudio | ALSA / PipeWire    |
//! | Hotkey       | `global-hotkey`| ✓       | ✓     | X11 only — see below   |
//! | Injection    | `enigo`        | SendInput | CGEvent | XTest / uinput     |
//! | Focus guard  | this crate     | UIA     | AX    | none — see below       |
//! | Autostart    | this crate     | registry | SMAppService | XDG autostart |
//!
//! # Wayland
//!
//! Wayland deliberately denies both of this app's core gestures: a background
//! client cannot register a global hotkey, and it cannot synthesise input into
//! another client. That is a security design, not a gap to work around. The
//! Linux build therefore targets X11 (and XWayland) for full function, and on a
//! Wayland session it degrades honestly rather than failing silently — see
//! [`Capability`].

pub mod resample;

use anyhow::Result;

/// What the current session actually permits, established at startup so the UI
/// can say so up front rather than after a take produces nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    pub global_hotkey: bool,
    pub inject_text: bool,
    /// Whether the focus guard can tell a text field from anything else.
    pub inspect_focus: bool,
    pub autostart: bool,
    /// Set when the session is one where the above are restricted by design.
    pub note: Option<String>,
}

/// A push-to-talk binding is held, not tapped, so both edges matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEdge {
    Pressed,
    Released,
}

/// Registers the push-to-talk binding and reports both edges.
pub trait HotkeyListener: Send {
    fn register(&mut self, hotkey: splaude_core::Hotkey) -> Result<()>;
    fn unregister(&mut self) -> Result<()>;
}

/// Posts synthetic keystrokes into whatever currently has focus.
///
/// Implementations must clear held modifiers before every event. Push-to-talk
/// means the binding's modifier is very likely physically down right now, and
/// on macOS Option+Delete deletes a whole word rather than a character — the
/// same class of bug exists on every platform.
pub trait Injector: Send {
    fn type_text(&mut self, text: &str) -> Result<()>;
    fn backspace(&mut self, count: usize) -> Result<()>;
}

/// Whether it is safe to type where focus currently is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusVerdict {
    /// A text surface that accepts input.
    Editable,
    /// Focus is on something that is not text — typing would fire shortcuts.
    NotEditable,
    /// The platform cannot tell. Treated as permission to proceed, because
    /// refusing every take on a platform with no introspection would make the
    /// app useless there.
    Unknown,
}

pub trait FocusGuard: Send {
    fn verdict(&self) -> FocusVerdict;
    /// Identifies the surface a take started in, so `anchor_input` can refuse
    /// to follow focus mid-sentence. `None` when the platform cannot tell.
    fn anchor(&self) -> Option<String>;
}

pub trait Autostart: Send {
    fn is_enabled(&self) -> bool;
    fn set(&self, enabled: bool) -> Result<()>;
}
