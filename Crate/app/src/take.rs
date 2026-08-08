//! One dictation, from key-down to committed text.
//!
//! A take owns three things that must start and stop together: the microphone,
//! the speech socket, and the task translating what comes back into keystrokes.
//! Dropping any one without the others leaves the mic open or the socket
//! half-closed, so they are created and torn down here as a unit.

use std::sync::mpsc::Sender;
use std::sync::Arc;
// Both belong to `probe`, which is gated with the tray it is reached from.
#[cfg(not(target_os = "linux"))]
use std::thread;
#[cfg(not(target_os = "linux"))]
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

use splaude_core::credential::Store;
use splaude_core::speech::{
    AnthropicSpeechBackend, Session, SpeechBackend, SpeechEvent, TranscriptBuffer,
};
use splaude_core::{diagnostic, Setting, Typer};
use splaude_platform::{focus, tone, Capture, FocusVerdict, Tone};

use crate::inject::Command;

/// What [`probe`] types. Short, unmistakable in a log, and unmistakable on
/// screen — someone running Test Paste needs to recognise what landed without
/// wondering whether it was their own typing.
#[cfg(not(target_os = "linux"))]
const PROBE_TEXT: &str = "splaude test paste";

/// How long [`probe`] waits before typing anything.
///
/// The menu item is clicked in a tray menu, which owns the keyboard focus while
/// it is open. Typing immediately would send the characters into a closing menu
/// rather than into the field the user is looking at. The same 0.6 s
/// `AppDelegate.swift` waits, for the same reason.
#[cfg(not(target_os = "linux"))]
const PROBE_SETTLE: Duration = Duration::from_millis(600);

pub struct Take {
    capture: Capture,
    session: Session,
    /// Remembered from the setting at start, so [`Take::finish`] does not have
    /// to be handed the whole setting again to know whether to sound.
    play_sound: bool,
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

        // Decided once, here, for the same reason the anchor is: it is a fact
        // about the application the take is aimed at, and re-asking per
        // keystroke would let a take change its mind halfway through and leave
        // half a sentence typed and half of it buffered.
        let keycode_app = is_keycode_app(setting);
        let live_typing = setting.live_typing && !keycode_app;

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
            live_typing,
            keycode_app,
            transcript,
        ));

        let feeding = session.clone();
        let capture = Capture::start(
            backend.audio_format(),
            Box::new(move |pcm| feeding.send_audio(pcm)),
            level,
        )
        .context("could not open the microphone")?;

        // Last, so the cue never sounds for a take that failed to open. An
        // acknowledgement the app did not earn is worse than no acknowledgement
        // — the whole reason to have one is to know the microphone is live.
        if setting.play_sound {
            tone::play(Tone::Start);
        }

        Ok(Self {
            capture,
            session,
            play_sound: setting.play_sound,
        })
    }

    /// Ends the take. The socket is told to close rather than dropped, so the
    /// server gets a chance to flush a trailing utterance.
    pub fn finish(mut self) {
        self.capture.stop();
        diagnostic::log(
            "take",
            format!("peak level {:.2}", self.capture.last_peak()),
        );
        if self.play_sound {
            tone::play(Tone::Stop);
        }
        self.session.finish();
    }
}

/// Types a known string into whatever has focus — injection alone, with no
/// microphone, no socket and no credential involved.
///
/// Every failure in this app looks the same from outside ("nothing happened"),
/// and injection is the part with the least verification behind it. This is how
/// someone tells "the mic never opened" from "the socket failed" from "the
/// keystrokes went nowhere", without speaking a word.
///
/// It is only worth that if it exercises what a take exercises, so it goes down
/// the same route: [`is_keycode_app`] decides between typing and pasting exactly
/// as a buffered take's delivery does, and the text travels the same
/// [`crate::inject`] channel to the same injector on the same thread.
///
/// Runs on a thread of its own because of [`PROBE_SETTLE`] — the caller is the
/// event loop, and sleeping there would freeze the tray menu mid-close.
///
/// Gated with the tray, which is its only entry point: a probe has to be
/// reachable to be worth anything, and the Linux build has no menu to reach it
/// from. Same `cfg` as `tray.rs` itself, for the same reason.
#[cfg(not(target_os = "linux"))]
pub fn probe(setting: Setting, injector: Sender<Command>) {
    let spawned = thread::Builder::new()
        .name("splaude-probe".into())
        .spawn(move || {
            thread::sleep(PROBE_SETTLE);

            // Read after the wait, not before: the point of the wait is that
            // focus has moved back out of the menu, so the question "which
            // application is this going to" only has an answer afterwards.
            let destination = focus::executable().unwrap_or_else(|| "unknown".to_string());
            let interval_micros = setting.typing_interval;
            let keycode_app = is_keycode_app(&setting);

            diagnostic::log(
                "test",
                format!(
                    "typing {PROBE_TEXT:?} into {destination} by {}",
                    if keycode_app { "paste" } else { "keystroke" }
                ),
            );

            let sent = injector.send(if keycode_app {
                Command::Paste {
                    text: PROBE_TEXT.to_string(),
                    interval_micros,
                }
            } else {
                Command::Type {
                    text: PROBE_TEXT.to_string(),
                    interval_micros,
                }
            });

            // The injector thread is gone, which means the app is on its way
            // out; say so rather than leave a probe that reported nothing.
            if sent.is_err() {
                diagnostic::log("test", "the injector is no longer running");
            }
        });

    if let Err(error) = spawned {
        diagnostic::log("test", format!("could not start the probe: {error}"));
    }
}

/// Turns transcript events into keystrokes for the life of one take.
async fn consume(
    mut event: UnboundedReceiver<SpeechEvent>,
    injector: Sender<Command>,
    setting: Setting,
    anchor: Option<String>,
    // Settled at take start rather than read off `setting`, because the target
    // application can veto live typing even when the user has it switched on.
    live_typing: bool,
    // Not the inverse of `live_typing`: a user with live typing switched off
    // buffers into an ordinary application too, and that take must be delivered
    // exactly as it always was. Only this says the destination cannot read
    // synthetic text at all.
    keycode_app: bool,
    on_transcript: Box<dyn Fn(String) + Send + 'static>,
) {
    let mut buffer = TranscriptBuffer::new();
    let mut typer = Typer::new();

    while let Some(what) = event.recv().await {
        match what {
            SpeechEvent::Open => diagnostic::log("take", "listening"),

            SpeechEvent::Transcribe { text, is_final } => {
                let display = buffer.apply(&text, is_final);

                if live_typing && may_type(&setting, anchor.as_deref()) {
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
    if !live_typing && may_type(&setting, anchor.as_deref()) {
        let interval_micros = setting.typing_interval;
        let text = committed.clone();

        // Paste only where typing provably cannot work. It stomps the user's
        // clipboard for a moment, so it stays the exception rather than
        // becoming everyone's delivery: an ordinary buffered take goes out the
        // same way it always has.
        let _ = injector.send(if keycode_app {
            Command::Paste {
                text,
                interval_micros,
            }
        } else {
            Command::Type {
                text,
                interval_micros,
            }
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

/// Whether the take is aimed at an application that re-encodes keystrokes by
/// keycode, and so must be pasted rather than typed.
///
/// Live typing is the worst possible way to feed such an app. Every keystroke we
/// send carries its character as a unicode payload on virtual key 0, and a
/// remote-desktop or VM client reads the keycode instead — so the far end gets
/// whatever key 0 is, once per character. Revising a word then sends backspaces
/// that land on text the user never sees, and the mess compounds for the length
/// of the take.
///
/// Buffering alone would not fix that — the same broken mechanism delivering
/// fifty characters at once is still fifty wrong characters. What fixes it is
/// the delivery: the buffered take goes out through [`Injector::paste`], which
/// carries the words on the clipboard and sends only `Ctrl+V` as keystrokes.
/// A real keycode is the one thing that survives re-encoding, which is exactly
/// how `TextInserter.swift` has always done it on macOS.
///
/// [`Injector::paste`]: splaude_platform::Injector::paste
///
/// Fails open: [`focus::executable`] answers `None` on macOS and Linux, which
/// have no way to name the foreground process yet, and there the take proceeds
/// exactly as it did before.
fn is_keycode_app(setting: &Setting) -> bool {
    let Some(executable) = focus::executable() else {
        return false;
    };

    if !setting.is_keycode_app(&executable) {
        return false;
    }

    // A user whose live typing silently stopped needs the log to say why.
    diagnostic::log(
        "take",
        format!(
            "{executable} re-encodes keystrokes by keycode — delivering this take \
             by paste at the end instead of typing as you speak"
        ),
    );
    true
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
