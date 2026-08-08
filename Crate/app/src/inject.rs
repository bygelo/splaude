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
}

/// Starts the injector thread. Dropping the returned sender ends it.
pub fn spawn() -> Result<Sender<Command>> {
    // Built here rather than on the thread so a missing permission (macOS
    // Accessibility, a Wayland session) surfaces as a startup error the user can
    // read, instead of a thread that silently does nothing.
    let mut injector = Injector::new().context("could not open synthetic input")?;

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
