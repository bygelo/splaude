//! Owns the synthetic-keystroke connection on a thread of its own.
//!
//! Two reasons it cannot just be called inline. The injector sleeps between
//! every keystroke — at the default interval a sentence is tens of milliseconds
//! of blocking — and doing that on the thread reading the socket would stall the
//! audio and the transcript behind the typing. And `Enigo` is not portable
//! across threads on every backend, so it is pinned to the one that built it and
//! spoken to through a channel.

use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{Context, Result};
use splaude_platform::Injector;

/// One edit, already decided by `splaude_core::Typer`.
#[derive(Debug)]
pub enum Command {
    Backspace {
        count: usize,
        interval_micros: u32,
    },
    Type {
        text: String,
        interval_micros: u32,
    },
    /// Deliver via the clipboard and a real paste chord, for an application
    /// that would mistranslate synthetic text. Carries the typing interval only
    /// so the fallback below has one when the paste does not go through.
    Paste {
        text: String,
        interval_micros: u32,
    },
    /// The push-to-talk chord moved. Sent by a settings reload, and the reason
    /// this is a command rather than a constructor argument: the injector must
    /// leave the *live* binding's modifier alone and clear every other one, so
    /// a rebind that never reached here would keep excluding a key the user no
    /// longer holds and start clearing one they do.
    ///
    /// Reload Settings is reached from the tray, which the Linux build does not
    /// have, so there this variant exists and nothing constructs it. Keeping the
    /// command shared rather than gating it means the injector answers the same
    /// vocabulary on every platform.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Bind {
        binding: splaude_core::Hotkey,
    },
}

/// Starts the injector thread. Dropping the returned sender ends it.
///
/// `binding` is the push-to-talk chord the hotkey listener is about to register.
/// The injector needs it to know which held modifier it must *not* release —
/// see `splaude_platform`'s injector for why releasing that one leaks the user's
/// held key into the take. It is passed here rather than sent as a command
/// because the thread below owns the injector from the moment it starts. A
/// later rebind reaches it as [`Command::Bind`], which is the same value
/// arriving down the same channel rather than a second way to set it.
pub fn spawn(binding: splaude_core::Hotkey) -> Result<Sender<Command>> {
    // Built here rather than on the thread so a missing permission (macOS
    // Accessibility, a Wayland session) surfaces as a startup error the user can
    // read, instead of a thread that silently does nothing.
    let mut injector = Injector::new().context("could not open synthetic input")?;
    injector.set_binding(binding);

    let (command, inbox) = mpsc::channel::<Command>();

    thread::Builder::new()
        .name("splaude-inject".into())
        .spawn(move || {
            for what in inbox {
                let outcome = match what {
                    Command::Backspace {
                        count,
                        interval_micros,
                    } => injector.backspace(count, interval_micros),
                    Command::Type {
                        text,
                        interval_micros,
                    } => injector.type_text(&text, interval_micros),

                    // Typing is the wrong delivery for this application — that
                    // is why the command exists — but wrong text beats no text,
                    // and a clipboard the OS refuses to hand over must not cost
                    // the user the take.
                    Command::Paste {
                        text,
                        interval_micros,
                    } => match injector.paste(&text) {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            splaude_core::diagnostic::log(
                                "type",
                                format!("paste failed, typing instead: {error:#}"),
                            );
                            injector.type_text(&text, interval_micros)
                        }
                    },

                    Command::Bind { binding } => {
                        injector.set_binding(binding);
                        Ok(())
                    }
                };

                // A failed keystroke loses a word; a panicking thread loses
                // every keystroke for the rest of the run.
                if let Err(error) = outcome {
                    splaude_core::diagnostic::log("type", format!("injection failed: {error}"));
                }
            }
        })
        .context("could not start the injector thread")?;

    Ok(command)
}
