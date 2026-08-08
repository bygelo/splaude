//! Portable core.
//!
//! Everything here compiles on every target and touches no OS integration
//! point. The macOS build proved these parts were already separable — the
//! Swift files that imported nothing but `Foundation` are exactly the ones
//! ported here, plus the live-typing diff, which was portable logic trapped
//! inside a file that also posted `CGEvent`s.

pub mod credential;
pub mod diagnostic;
pub mod quota;
pub mod setting;
pub mod speech;
pub mod typer;
pub mod update;

pub use credential::{Credential, CredentialError, CredentialSource, Health, Store};
pub use setting::{Code, Hotkey, Modifiers, Setting};
pub use speech::{SpeechAudioFormat, SpeechEvent, TranscriptBuffer};
pub use typer::{TypeAction, Typer};
