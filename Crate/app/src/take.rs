//! One dictation, from key-down to committed text.
//!
//! A take owns three things that must start and stop together: the microphone,
//! the speech socket, and the task translating what comes back into keystrokes.
//! Dropping any one without the others leaves the mic open or the socket
//! half-closed, so they are created and torn down here as a unit.

use std::sync::atomic::{AtomicBool, Ordering};
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
use splaude_core::speech::deepgram::DeepgramSpeechBackend;
use splaude_core::speech::{
    AnthropicSpeechBackend, Session, SpeechBackend, SpeechEvent, TranscriptBuffer,
};
use splaude_core::{diagnostic, replacement, Setting, Typer};
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
    /// Set by [`Take::discard`], read by the consumer task before it inserts
    /// anything.
    ///
    /// Shared rather than passed at the end because by the time a take is
    /// cancelled the consumer is already running on another thread, waiting on
    /// a socket that has not finished flushing — there is nobody to hand a
    /// decision to. The flag is what the consumer checks when the words finally
    /// arrive.
    cancelled: Arc<AtomicBool>,
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
        // Where the take began. Everything typed later is checked against this,
        // so changing window mid-sentence cannot spray the rest of a dictation
        // — backspaces included — into whatever you switched to.
        let anchor = focus::anchor();

        // Decided once, here, for the same reason the anchor is: it is a fact
        // about the application the take is aimed at, and re-asking per
        // keystroke would let a take change its mind halfway through and leave
        // half a sentence typed and half of it buffered.
        let keycode_app = is_keycode_app(setting);
        let buffer_only = is_buffer_only_app(setting);
        let live_typing = setting.live_typing && !keycode_app && !buffer_only;

        // The one place a provider is chosen, and the reason the credential is
        // loaded inside the branch rather than above it: only the Anthropic
        // path needs a Claude Code credential, and refusing to start a Deepgram
        // take because `claude` was never run would be the abstraction leaking
        // in the most annoying possible direction.
        let backend: Box<dyn SpeechBackend> = match setting.speech_backend.trim() {
            "deepgram" => Box::new(DeepgramSpeechBackend::new(setting)?),

            // Anything unrecognised falls back rather than failing. A typo in
            // the settings file must not cost the user dictation — it costs
            // them a line in the log and the behaviour they had before.
            other => {
                if !other.is_empty() && other != "anthropic" {
                    diagnostic::log(
                        "take",
                        format!(
                            "unknown speechBackend {other:?} — using anthropic.                              Known backends: anthropic, deepgram"
                        ),
                    );
                }
                let credential = store.load().context("no usable Claude Code credential")?;
                Box::new(AnthropicSpeechBackend::new(credential, setting).with_store(store))
            }
        };
        let (event, inbox) = tokio::sync::mpsc::unbounded_channel();

        // `start` spawns the socket task, so it needs a runtime in context.
        let session = {
            let _guard = runtime.enter();
            backend
                .start(event)
                .context("could not open the speech socket")?
        };

        let Report { level, transcript } = report;
        let cancelled = Arc::new(AtomicBool::new(false));

        runtime.spawn(consume(
            inbox,
            injector,
            setting.clone(),
            anchor,
            Delivery {
                live_typing,
                keycode_app,
                cancelled: Arc::clone(&cancelled),
            },
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
            cancelled,
        })
    }

    /// Ends the take and throws away whatever it heard.
    ///
    /// Only meaningful for what has not been inserted yet, and that limit is
    /// worth being plain about: a live-typed take has already put words on
    /// screen and this cannot unsay them. What it does prevent is the *rest* of
    /// the take landing — which is the case it exists for, realising halfway
    /// through that the words are going into the wrong window.
    pub fn discard(self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.finish();
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
/// How one take is to be delivered, settled the moment it starts.
///
/// Grouped rather than passed as four arguments because they are one decision
/// with four parts, all made at the same instant and none re-asked afterwards:
/// re-deciding mid-take is precisely the bug the comments below warn about.
struct Delivery {
    /// Settled at take start rather than read off `setting`, because the target
    /// application can veto live typing even when the user has it switched on.
    live_typing: bool,
    /// Not the inverse of `live_typing`: a user with live typing switched off
    /// buffers into an ordinary application too, and that take must be
    /// delivered exactly as it always was. Only this says the destination
    /// cannot read synthetic text at all.
    keycode_app: bool,
    /// Set if the take is cancelled while it is still running.
    cancelled: Arc<AtomicBool>,
}

async fn consume(
    mut event: UnboundedReceiver<SpeechEvent>,
    injector: Sender<Command>,
    setting: Setting,
    anchor: Option<String>,
    delivery: Delivery,
    on_transcript: Box<dyn Fn(String) + Send + 'static>,
) {
    let Delivery {
        live_typing,
        keycode_app,
        cancelled,
    } = delivery;
    let mut buffer = TranscriptBuffer::new();
    let mut typer = Typer::new();

    // When the socket opened, and whether anything has come back yet. Together
    // they answer the only latency question a person asks about dictation:
    // "how long after I start talking do words appear?"
    //
    // Measured rather than tuned, and that ordering is the point. Endpointing
    // is reported to be the largest single contributor to perceived latency in
    // streaming recognition, and splaude sets `endpointing_ms` on the wire — but
    // it had never recorded a number of its own, so any change to that setting
    // would have been guessing at whether it helped. This is the number to
    // watch, in the log, on a real take.
    let mut opened: Option<std::time::Instant> = None;
    let mut first_words = false;

    while let Some(what) = event.recv().await {
        match what {
            SpeechEvent::Open => {
                opened = Some(std::time::Instant::now());
                diagnostic::log("take", "listening");
            }

            SpeechEvent::Transcribe { text, is_final } => {
                // Once per take, on the first thing the recogniser says.
                if !first_words {
                    first_words = true;
                    if let Some(opened) = opened {
                        diagnostic::log(
                            "take",
                            format!(
                                "first words {} ms after the socket opened",
                                opened.elapsed().as_millis()
                            ),
                        );
                    }
                }

                // Corrections are applied to the *target* the typer diffs
                // against, not to the keystrokes it emits. That is what makes
                // them safe on a live take: the typer's whole model is "what
                // should be on screen", so a revised interim is compared
                // against corrected text and the diff removes exactly what the
                // correction changed. Applying them after the diff would leave
                // the typer believing it had typed the uncorrected words, and
                // the next revision would measure against a screen that never
                // existed.
                let display =
                    replacement::apply(&buffer.apply(&text, is_final), &setting.replacement);

                if live_typing
                    && !cancelled.load(Ordering::SeqCst)
                    && may_type(&setting, anchor.as_deref())
                {
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

    // Nothing is inserted and nothing is reported: a cancelled take is one the
    // user has said they do not want, and handing its words to the tray for
    // copying would be keeping them anyway.
    if cancelled.load(Ordering::SeqCst) {
        diagnostic::log("take", "cancelled — nothing was inserted");
        return;
    }

    // The same correction as the live path above, for the take as a whole —
    // buffered mode never went through the typer, so this is its only chance.
    let committed = replacement::apply(buffer.committed(), &setting.replacement);
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
/// Whether the take is aimed at an application the user has asked to have
/// buffered rather than typed into live.
///
/// Separate from [`is_keycode_app`] because the two answer different questions
/// and one of them is not a defect: a keycode application *cannot* be typed
/// into, while a buffered one merely should not be. Keeping them apart means
/// the log can say which it was, and means turning off the built-in
/// remote-desktop list does not also throw away the user's own terminal rule.
///
/// Fails open on a platform that cannot name the foreground process, exactly as
/// the keycode carve-out does.
fn is_buffer_only_app(setting: &Setting) -> bool {
    let Some(executable) = focus::executable() else {
        return false;
    };

    if !setting.is_buffer_only_app(&executable) {
        return false;
    }

    diagnostic::log(
        "take",
        format!("{executable} is on the buffer-only list — pasting this take at the end"),
    );
    true
}

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

#[cfg(test)]
mod test {
    use super::*;

    /// These drive [`consume`] directly, which is the seam immediately below
    /// the microphone.
    ///
    /// The reason to test here rather than through `Take::start` is not
    /// convenience — it is that the machine this was written on has no input
    /// device at all, and neither does a CI runner. Everything above this seam
    /// (open a socket, open a microphone, keep them alive) needs hardware and a
    /// credential; everything below it is the part that decides what the user
    /// actually sees, and it needs neither. A take is a stream of
    /// [`SpeechEvent`] turning into a stream of [`Command`], and that is
    /// exactly what these hand it and read back.
    ///
    /// What they do NOT prove: that a real microphone produces those events, or
    /// that the commands reach a real application. `tests/injection.rs` in
    /// `splaude-platform` covers the second half against a real window.
    fn setting() -> Setting {
        Setting {
            // Both guards consult the live desktop — which window has focus,
            // and whether it is a text field — so leaving them on would make
            // every assertion here depend on what the developer happened to be
            // looking at. They have their own tests.
            guard_focus: false,
            anchor_input: false,
            typing_interval: 0,
            ..Setting::default()
        }
    }

    /// Runs a take made of `event`, and hands back every command it produced
    /// and every transcript it reported.
    fn run(
        setting: Setting,
        delivery: Delivery,
        event: Vec<SpeechEvent>,
    ) -> (Vec<Command>, Vec<String>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");

        let (speech_tx, speech_rx) = tokio::sync::mpsc::unbounded_channel();
        for what in event {
            speech_tx.send(what).expect("queue event");
        }
        // Dropping the sender is what ends the take, standing in for the socket
        // closing after `CloseStream`.
        drop(speech_tx);

        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let said = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let heard = std::sync::Arc::clone(&said);

        runtime.block_on(consume(
            speech_rx,
            command_tx,
            setting,
            None,
            delivery,
            Box::new(move |text| heard.lock().expect("heard").push(text)),
        ));

        let command: Vec<Command> = command_rx.try_iter().collect();
        let said = said.lock().expect("said").clone();
        (command, said)
    }

    fn delivery(live_typing: bool, keycode_app: bool) -> Delivery {
        Delivery {
            live_typing,
            keycode_app,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn interim(text: &str) -> SpeechEvent {
        SpeechEvent::Transcribe {
            text: text.to_string(),
            is_final: false,
        }
    }

    fn final_(text: &str) -> SpeechEvent {
        SpeechEvent::Transcribe {
            text: text.to_string(),
            is_final: true,
        }
    }

    /// What the commands would leave on screen.
    ///
    /// Backspaces are applied rather than ignored, which is the whole point:
    /// live typing corrects itself by deleting, so a helper that concatenated
    /// only the `Type` commands would read "Cloudaude Code" where a user sees
    /// "Claude Code" and would call a working revision a bug. (It did, on the
    /// first run of these tests.)
    fn screen(command: &[Command]) -> String {
        let mut on_screen = String::new();

        for what in command {
            match what {
                Command::Type { text, .. } => on_screen.push_str(text),
                Command::Backspace { count, .. } => {
                    for _ in 0..*count {
                        on_screen.pop();
                    }
                }
                _ => {}
            }
        }

        on_screen
    }

    fn pasted(command: &[Command]) -> Option<String> {
        command.iter().find_map(|what| match what {
            Command::Paste { text, .. } => Some(text.clone()),
            _ => None,
        })
    }

    #[test]
    fn a_buffered_take_types_the_whole_thing_once_at_the_end() {
        let (command, said) = run(
            setting(),
            delivery(false, false),
            vec![final_("hello there")],
        );

        assert_eq!(screen(&command), "hello there");
        // Buffered means one delivery, not a stream of them.
        assert_eq!(command.len(), 1);
        assert_eq!(said, vec!["hello there".to_string()]);
    }

    /// The carve-out that exists because a remote-desktop window cannot read
    /// synthetic text at all. Its delivery must be the clipboard, not keys.
    #[test]
    fn a_keycode_application_is_pasted_and_never_typed() {
        let (command, _) = run(
            setting(),
            delivery(false, true),
            vec![final_("hello there")],
        );

        assert_eq!(pasted(&command).as_deref(), Some("hello there"));
        assert!(screen(&command).is_empty(), "{command:?}");
    }

    /// Live typing emits as the words arrive rather than waiting for the end —
    /// the property the whole app is built around.
    #[test]
    fn a_live_take_types_before_the_utterance_ends() {
        let (command, _) = run(
            setting(),
            delivery(true, false),
            vec![interim("hello"), final_("hello there")],
        );

        assert_eq!(screen(&command), "hello there");
        // More than one command, because the first interim went out on its own.
        assert!(command.len() > 1, "{command:?}");
    }

    /// A revision must remove exactly what changed and no more. This is the
    /// diff the live-typing model exists for, exercised end to end rather than
    /// as a pure function.
    #[test]
    fn a_revised_word_is_backspaced_rather_than_appended() {
        let (command, _) = run(
            setting(),
            delivery(true, false),
            vec![interim("hello wold"), interim("hello world")],
        );

        let removed: usize = command
            .iter()
            .filter_map(|what| match what {
                Command::Backspace { count, .. } => Some(*count),
                _ => None,
            })
            .sum();

        // Something was deleted, the whole word was not, and what is left on
        // screen is the corrected sentence.
        assert!(removed > 0, "nothing was backspaced: {command:?}");
        assert!(removed <= 4, "a whole word or more went: {command:?}");
        assert_eq!(screen(&command), "hello world", "{command:?}");
    }

    /// The corrections map, applied where it has to be applied: to the text the
    /// typer diffs against, so a revision cannot double-apply it.
    #[test]
    fn a_correction_reaches_the_keystrokes_of_a_live_take() {
        let corrected = Setting {
            replacement: [("Cloud Code".to_string(), "Claude Code".to_string())]
                .into_iter()
                .collect(),
            ..setting()
        };

        let (command, said) = run(
            corrected,
            delivery(true, false),
            vec![
                interim("Cloud"),
                interim("Cloud Code"),
                final_("Cloud Code is good"),
            ],
        );

        assert_eq!(screen(&command), "Claude Code is good", "{command:?}");
        // And the copy offered to the user is corrected too, not the raw one.
        assert_eq!(said, vec!["Claude Code is good".to_string()]);
    }

    #[test]
    fn a_correction_reaches_a_buffered_take_as_well() {
        let corrected = Setting {
            replacement: [("Cloud Code".to_string(), "Claude Code".to_string())]
                .into_iter()
                .collect(),
            ..setting()
        };

        let (command, _) = run(
            corrected,
            delivery(false, false),
            vec![final_("Cloud Code")],
        );
        assert_eq!(screen(&command), "Claude Code");
    }

    /// Cancelling is not stopping. Nothing may be inserted, and nothing may be
    /// offered for copying either — the words are ones the user said they did
    /// not want.
    #[test]
    fn a_cancelled_take_inserts_nothing_and_reports_nothing() {
        let cancelled = Delivery {
            live_typing: false,
            keycode_app: false,
            cancelled: Arc::new(AtomicBool::new(true)),
        };

        let (command, said) = run(setting(), cancelled, vec![final_("never mind")]);

        assert!(command.is_empty(), "{command:?}");
        assert!(said.is_empty(), "{said:?}");
    }

    /// Cancelling mid-take stops the *rest* of it. What is already on screen
    /// cannot be unsaid — that limit is documented on `Take::discard` — but
    /// nothing further may be typed.
    #[test]
    fn cancelling_a_live_take_stops_the_words_still_to_come() {
        let flag = Arc::new(AtomicBool::new(false));
        let delivery = Delivery {
            live_typing: true,
            keycode_app: false,
            cancelled: Arc::clone(&flag),
        };

        // Set before the run, which is the same thing the loop does from
        // another thread while the socket is still flushing.
        flag.store(true, Ordering::SeqCst);

        let (command, said) = run(
            setting(),
            delivery,
            vec![interim("half a"), final_("half a sentence")],
        );

        assert!(command.is_empty(), "{command:?}");
        assert!(said.is_empty(), "{said:?}");
    }

    /// An empty take is one the recogniser heard nothing in. It must not paste
    /// an empty string or report one.
    #[test]
    fn a_take_with_no_words_delivers_nothing() {
        let (command, said) = run(setting(), delivery(false, false), vec![]);
        assert!(command.is_empty(), "{command:?}");
        assert!(said.is_empty(), "{said:?}");
    }

    /// A failure frame is not the end of a take — the socket may recover, and
    /// the words already committed are still the user's.
    #[test]
    fn a_non_fatal_failure_does_not_lose_what_was_already_said() {
        let (command, said) = run(
            setting(),
            delivery(false, false),
            vec![
                final_("first part"),
                SpeechEvent::Fail {
                    message: "hiccup".into(),
                    fatal: false,
                },
            ],
        );

        assert_eq!(screen(&command), "first part");
        assert_eq!(said, vec!["first part".to_string()]);
    }
}
