//! Talking to the copy of splaude that is already running.
//!
//! Two problems, one mechanism, which is why they are in the same module.
//!
//! **A second launch used to do nothing, visibly.** splaude has no window, and
//! when started from Explorer or from launch-at-login it has no console either.
//! The second process would start, fail to register the hotkey — the binding is
//! already held by the first — and exit. From outside, double-clicking the icon
//! did nothing at all, and doing it again did nothing again.
//!
//! **The hotkey is not always deliverable.** A full-screen remote-desktop
//! client takes the keyboard before any local application sees it, so the one
//! way to start a take can be unreachable precisely when someone is trying to
//! dictate into it. Anything that can run a command — a launcher, a Stream
//! Deck, AutoHotkey, the remote host itself — can now start one instead.
//!
//! The mechanism is a per-user channel that the running instance listens on.
//! Creating it is also the single-instance test, and deliberately so: asking
//! "is anyone there?" and "please take this" are the same question, and a
//! separate mutex would be a second source of truth about whether splaude is
//! running.
//!
//! It carries three fixed words and nothing else. It is not an API and should
//! not become one — a local channel that accepts arbitrary instructions is a
//! way for anything running as this user to drive the keyboard.

use std::str::FromStr;

use anyhow::Result;

/// What one process can ask another to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Come to the front. What a second launch means.
    Show,
    /// Start a take, or end the one running — the same thing the tray's
    /// Start Dictation and the floating button ask for.
    Toggle,
    /// End the take without inserting anything.
    Cancel,
}

impl Command {
    /// The word on the wire. Fixed strings, not a serialisation format: the
    /// channel is deliberately not extensible.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Toggle => "toggle",
            Self::Cancel => "cancel",
        }
    }
}

impl FromStr for Command {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "show" => Ok(Self::Show),
            "toggle" => Ok(Self::Toggle),
            "cancel" => Ok(Self::Cancel),
            _ => Err(()),
        }
    }
}

/// What happened when a command was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A running instance took it.
    Taken,
    /// Nobody is listening — this is the only splaude on this machine.
    NobodyListening,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn every_command_round_trips_through_its_word() {
        for command in [Command::Show, Command::Toggle, Command::Cancel] {
            assert_eq!(command.word().parse::<Command>(), Ok(command));
        }
    }

    /// The channel accepts three words. Anything else is refused rather than
    /// guessed at — it is reachable by anything running as this user, and the
    /// thing on the other end drives the keyboard.
    #[test]
    fn an_unknown_word_is_refused() {
        for word in ["", "quit", "type rm -rf", "SHOW", "toggle now"] {
            assert!(word.parse::<Command>().is_err(), "{word}");
        }
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        // The wire ends every message with a newline.
        assert_eq!("toggle\n".parse::<Command>(), Ok(Command::Toggle));
        assert_eq!("  show  ".parse::<Command>(), Ok(Command::Show));
    }
}

// MARK: - Windows

#[cfg(windows)]
mod backend {
    use std::ffi::c_void;

    use anyhow::{anyhow, Result};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::{Command, Delivery};

    /// Longest message the channel will read. The vocabulary is three words;
    /// this is a bound on what a caller can make the listener allocate, not a
    /// size anything legitimate approaches.
    const MESSAGE_LIMIT: usize = 64;

    /// The channel's name, per user.
    ///
    /// Named pipes live in one namespace for the whole machine, so a name
    /// without the user in it would be shared across everyone signed in — and
    /// then one person's second launch would raise another person's window, or
    /// worse, start a take on their session. `USERNAME` is not a security
    /// boundary and is not being used as one; the pipe's own default ACL is
    /// what keeps other users out. This only stops two legitimate users from
    /// colliding by accident.
    fn name() -> HSTRING {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        let user: String = user
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect();
        HSTRING::from(format!(r"\\.\pipe\splaude.control.{user}"))
    }

    /// Claims the channel, or reports that someone else holds it.
    ///
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` is what makes this the single-instance
    /// test rather than merely a listener: with it, creating a pipe whose name
    /// already exists fails instead of quietly adding a second instance. The
    /// claim is therefore atomic, and two copies of splaude racing at login
    /// cannot both win it.
    pub fn claim() -> Result<Option<Channel>> {
        // SAFETY: the name outlives the call, and every other argument is a
        // constant. A failure is returned as a null-ish handle the binding
        // reports as an error rather than something to dereference.
        let handle = unsafe {
            CreateNamedPipeW(
                &name(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                // One instance: the listener serves one caller at a time, and
                // there is never more than one caller.
                1,
                MESSAGE_LIMIT as u32,
                MESSAGE_LIMIT as u32,
                0,
                None,
            )
        };

        // An invalid handle is the answer the caller wants rather than an
        // error: it is what "somebody already holds this" looks like, and it is
        // what every second launch sees.
        if handle.is_invalid() {
            Ok(None)
        } else {
            Ok(Some(Channel { handle }))
        }
    }

    /// The listening end, owned by the instance that claimed it.
    pub struct Channel {
        handle: HANDLE,
    }

    // SAFETY: a pipe handle is a kernel object with no thread affinity, and the
    // only thing that ever touches this one is the listener thread it is moved
    // to.
    unsafe impl Send for Channel {}

    impl Channel {
        /// Serves callers until the process ends.
        ///
        /// Blocking, and meant to be given a thread of its own. There is no
        /// shutdown path because there is nothing to shut down: the listener
        /// holds no state, and the process exiting closes the handle.
        pub fn serve(self, on_command: impl Fn(Command)) {
            loop {
                // SAFETY: the handle is live for the whole loop and owned here.
                // `ConnectNamedPipe` blocks until a caller arrives; an error
                // means the pipe is unusable, so the listener stands down
                // rather than spinning on it.
                let connected = unsafe { ConnectNamedPipe(self.handle, None) };
                if connected.is_err() {
                    // ERROR_PIPE_CONNECTED is not a failure: it means a
                    // caller arrived in the gap between creating the pipe and
                    // connecting to it, so there is already somebody to read.
                    let already = connected.as_ref().err().is_some_and(|error| {
                        error.code().0 as u32 & 0xFFFF == ERROR_PIPE_CONNECTED.0
                    });
                    if !already {
                        return;
                    }
                }

                let mut buffer = [0u8; MESSAGE_LIMIT];
                let mut read = 0u32;
                // SAFETY: the buffer is a live local and the count is written
                // back through a pointer to another live local.
                let got =
                    unsafe { ReadFile(self.handle, Some(&mut buffer), Some(&mut read), None) };

                if got.is_ok() && read > 0 {
                    if let Ok(command) = std::str::from_utf8(&buffer[..read as usize])
                        .unwrap_or_default()
                        .parse::<Command>()
                    {
                        on_command(command);
                        // Answered so the caller knows it was taken rather than
                        // timing out and reporting a failure that did not
                        // happen.
                        let mut written = 0u32;
                        // SAFETY: as above.
                        let _ = unsafe {
                            WriteFile(self.handle, Some(b"ok"), Some(&mut written), None)
                        };
                    }
                }

                // SAFETY: disconnecting a pipe this process owns; the next
                // iteration reconnects the same handle for the next caller.
                let _ = unsafe { DisconnectNamedPipe(self.handle) };
            }
        }
    }

    impl Drop for Channel {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateNamedPipeW`, is owned here,
            // and is not used again.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    /// Hands a command to the running instance.
    pub fn send(command: Command) -> Result<Delivery> {
        // SAFETY: the name outlives the call; the rest are constants.
        let pipe = unsafe {
            CreateFileW(
                &name(),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        };

        let pipe = match pipe {
            Ok(pipe) => pipe,
            Err(error) => {
                let code = error.code().0 as u32 & 0xFFFF;
                // Nothing there is the ordinary answer, not a failure: it is
                // what every first launch sees.
                if code == ERROR_FILE_NOT_FOUND.0 || code == ERROR_PIPE_BUSY.0 {
                    return Ok(Delivery::NobodyListening);
                }
                return Err(anyhow!("could not reach the running splaude: {error}"));
            }
        };

        let message = format!("{}\n", command.word());
        let mut written = 0u32;
        // SAFETY: the message outlives the call and the count is written back
        // through a pointer to a live local.
        let sent = unsafe { WriteFile(pipe, Some(message.as_bytes()), Some(&mut written), None) };

        // Read the acknowledgement so the caller does not exit before the
        // listener has acted on it — closing the pipe first can lose the write.
        let mut answer = [0u8; 8];
        let mut read = 0u32;
        // SAFETY: as above.
        let _ = unsafe { ReadFile(pipe, Some(&mut answer), Some(&mut read), None) };

        // SAFETY: the handle came from `CreateFileW` and is not used again.
        let _ = unsafe { CloseHandle(pipe) };

        sent.map(|()| Delivery::Taken)
            .map_err(|error| anyhow!("could not send to the running splaude: {error}"))
    }

    /// Unused on Windows; the pipe needs no path.
    #[allow(dead_code)]
    fn unused(_: *const c_void) {}
}

// MARK: - Unix

#[cfg(unix)]
mod backend {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    use anyhow::{anyhow, Result};

    use super::{Command, Delivery};

    const MESSAGE_LIMIT: usize = 64;

    /// The socket's path, per user by virtue of living in the user's own
    /// runtime directory.
    fn path() -> PathBuf {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
        base.join(format!("splaude.control.{user}.sock"))
    }

    /// Claims the channel, or reports that someone else holds it.
    ///
    /// A socket file outlives the process that made it — a crash or a kill
    /// leaves one behind — so an existing path is not proof anyone is home.
    /// The test is whether it can be *connected to*: a refused connection means
    /// the file is a corpse, and it is removed and rebound.
    pub fn claim() -> Result<Option<Channel>> {
        let path = path();

        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Ok(None);
            }
            let _ = std::fs::remove_file(&path);
        }

        match UnixListener::bind(&path) {
            Ok(listener) => Ok(Some(Channel { listener })),
            // Lost a race with another instance binding it first.
            Err(_) => Ok(None),
        }
    }

    pub struct Channel {
        listener: UnixListener,
    }

    impl Channel {
        pub fn serve(self, on_command: impl Fn(Command)) {
            for stream in self.listener.incoming() {
                let Ok(mut stream) = stream else { continue };

                let mut buffer = [0u8; MESSAGE_LIMIT];
                let Ok(read) = stream.read(&mut buffer) else {
                    continue;
                };

                if let Ok(command) = std::str::from_utf8(&buffer[..read])
                    .unwrap_or_default()
                    .parse::<Command>()
                {
                    on_command(command);
                    let _ = stream.write_all(b"ok");
                }
            }
        }
    }

    pub fn send(command: Command) -> Result<Delivery> {
        let path = path();

        let mut stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(_) => return Ok(Delivery::NobodyListening),
        };

        stream
            .write_all(format!("{}\n", command.word()).as_bytes())
            .map_err(|error| anyhow!("could not send to the running splaude: {error}"))?;

        // Wait for the acknowledgement, so the caller does not exit before the
        // listener has acted.
        let mut answer = [0u8; 8];
        let _ = stream.read(&mut answer);

        Ok(Delivery::Taken)
    }
}

// MARK: - Neither

#[cfg(not(any(windows, unix)))]
mod backend {
    use anyhow::{anyhow, Result};

    use super::{Command, Delivery};

    pub struct Channel;

    impl Channel {
        pub fn serve(self, _on_command: impl Fn(Command)) {}
    }

    pub fn claim() -> Result<Option<Channel>> {
        Ok(None)
    }

    pub fn send(_command: Command) -> Result<Delivery> {
        Err(anyhow!("no control channel on this platform"))
    }
}

pub use backend::Channel;

/// Claims the control channel for this process.
///
/// `Ok(None)` means another splaude already holds it — which is the
/// single-instance answer, and the only one this app needs.
pub fn claim() -> Result<Option<Channel>> {
    backend::claim()
}

/// Hands a command to the running instance, if there is one.
pub fn send(command: Command) -> Result<Delivery> {
    backend::send(command)
}
