//! One dictation, from key-down to committed text.
//!
//! A take owns three things that must start and stop together: the microphone,
//! the speech socket, and the task translating what comes back into keystrokes.
//! Dropping any one without the others leaves the mic open or the socket
//! half-closed, so they are created and torn down here as a unit.

use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

use splaude_core::credential::Store;
use splaude_core::speech::{
    AnthropicSpeechBackend, Session, SpeechBackend, SpeechEvent, TranscriptBuffer,
};
use splaude_core::{diagnostic, Setting, Typer};
use splaude_platform::{focus, Capture, FocusVerdict};

use crate::inject::Command;

pub struct Take {
    capture: Capture,
    session: Session,
}

/// Where a take tells the interface what it is doing.
///
/// Both callbacks fire on threads the take owns — the level on the audio
/// callback, the transcript on the socket task — so neither may touch the tray,
/// which belongs to the main thread. They exist to hand a value to whoever can.
pub struct Report {
    /// Smoothed 0…1 input level, once per audio buffer. Hundreds of times a
    /// second: whatever this does has to be cheap or has to filter.
    pub level: Box<dyn Fn(f32) + Send + 'static>,
    /// The take's committed text, once, when the socket closes. Never called
    /// with an empty string — a take that produced nothing has nothing to say.
    pub transcript: Box<dyn Fn(String) + Send + 'static>,
}

// Only the Linux build, which has no tray, has nobody to report to; everywhere
// else a take always has a real destination, so this would be dead code.
#[cfg(target_os = "linux")]
impl Report {
    /// Nobody is listening.
    pub fn silent() -> Self {
        Self {
            level: Box::new(|_level| {}),
            transcript: Box::new(|_text| {}),
        }
    }
}

impl Take {
    /// Opens the socket and the microphone. Returns once both are live, so a
    /// failure here means nothing was started rather than half of it.
    pub fn start(
        runtime: &Handle,
        store: Arc<Store>,
        setting: &Setting,
        injector: Sender<Command>,
        report: Report,
    ) -> Result<Self> {
        let credential = store.load().context("no usable Claude Code credential")?;

        // Where the take began. Everything typed later is checked against this,
        // so changing window mid-sentence cannot spray the rest of a dictation
        // — backspaces included — into whatever you switched to.
        let anchor = focus::anchor();

        let backend = AnthropicSpeechBackend::new(credential, setting).with_store(store);
        let (event, inbox) = tokio::sync::mpsc::unbounded_channel();

        // `start` spawns the socket task, so it needs a runtime in context.
        let session = {
            let _guard = runtime.enter();
            backend
                .start(event)
                .context("could not open the speech socket")?
        };

        let Report { level, transcript } = report;
        runtime.spawn(consume(
            inbox,
            injector,
            setting.clone(),
            anchor,
            transcript,
        ));

        let feeding = session.clone();
        let capture = Capture::start(
            backend.audio_format(),
            Box::new(move |pcm| feeding.send_audio(pcm)),
            level,
        )
        .context("could not open the microphone")?;

        Ok(Self { capture, session })
    }

    /// Ends the take. The socket is told to close rather than dropped, so the
    /// server gets a chance to flush a trailing utterance.
    pub fn finish(mut self) {
        self.capture.stop();
        diagnostic::log(
            "take",
            format!("peak level {:.2}", self.capture.last_peak()),
        );
        self.session.finish();
    }
}

/// Turns transcript events into keystrokes for the life of one take.
async fn consume(
    mut event: UnboundedReceiver<SpeechEvent>,
    injector: Sender<Command>,
    setting: Setting,
    anchor: Option<String>,
    on_transcript: Box<dyn Fn(String) + Send + 'static>,
) {
    let mut buffer = TranscriptBuffer::new();
    let mut typer = Typer::new();

    while let Some(what) = event.recv().await {
        match what {
            SpeechEvent::Open => diagnostic::log("take", "listening"),

            SpeechEvent::Transcribe { text, is_final } => {
                let display = buffer.apply(&text, is_final);

                if setting.live_typing && may_type(&setting, anchor.as_deref()) {
                    if let Some(action) = typer.update(&display) {
                        emit(&injector, action, setting.typing_interval);
                    }
                }

                if is_final {
                    typer.lock();
                }
            }

            SpeechEvent::Fail { message, fatal } => {
                diagnostic::log(
                    "take",
                    format!("{}{message}", if fatal { "fatal: " } else { "" }),
                );
            }

            SpeechEvent::Close => break,
        }
    }

    let committed = buffer.committed().to_string();
    if committed.is_empty() {
        return;
    }

    // Buffered mode types nothing until the end, so the whole take lands here.
    // Live typing has already emitted it.
    if !setting.live_typing && may_type(&setting, anchor.as_deref()) {
        let _ = injector.send(Command::Type {
            text: committed.clone(),
            interval_micros: setting.typing_interval,
        });
    }

    // Reported whether or not it was typed. Text the focus guard refused is
    // exactly the text a user most wants a copy of.
    on_transcript(committed);
}

fn emit(injector: &Sender<Command>, action: splaude_core::TypeAction, interval_micros: u32) {
    if action.remove_count > 0 {
        let _ = injector.send(Command::Backspace {
            count: action.remove_count,
            interval_micros,
        });
    }
    if !action.addition.is_empty() {
        let _ = injector.send(Command::Type {
            text: action.addition,
            interval_micros,
        });
    }
}

/// Whether it is safe to type right now.
///
/// Both checks fail open. The focus guard only refuses when the platform is
/// *certain* the surface is not text — `Unknown` proceeds, or the app would be
/// unusable everywhere the platform cannot introspect. The anchor only refuses
/// when it knows where the take began and can see that focus has moved since.
fn may_type(setting: &Setting, anchor: Option<&str>) -> bool {
    if setting.guard_focus && focus::verdict() == FocusVerdict::NotEditable {
        diagnostic::log("focus", "not a text surface — holding");
        return false;
    }

    if setting.anchor_input {
        if let (Some(started), Some(now)) = (anchor, focus::anchor()) {
            if started != now {
                diagnostic::log("focus", "focus left the field — holding");
                return false;
            }
        }
    }

    true
}
