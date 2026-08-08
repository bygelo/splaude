//! Append-only log, plus stderr when run from a terminal.
//!
//! A tray app has nowhere to print, and every failure mode here (permission,
//! socket, empty audio, refused injection) looks identical from the outside —
//! "nothing happened".

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Most recent lines, so the tray menu can show them without opening a log
/// viewer. Bounded — this is a breadcrumb trail, not a second copy of the file.
const RECENT_LIMIT: usize = 40;

fn recent() -> &'static Mutex<Vec<String>> {
    static RECENT: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    RECENT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Where the log lives, per platform convention.
///
/// macOS keeps the original `~/Library/Logs/splaude.log` so an upgrade from the
/// Swift build finds its own history. Windows and Linux follow their own norms
/// rather than inventing a `Library` directory that means nothing there.
/// Overrides where the log is written. A full file path, not a directory.
///
/// For a portable install that wants its log beside the binary, and for anyone
/// diagnosing a machine where the usual location is not writable.
pub const PATH_OVERRIDE: &str = "SPLAUDE_LOG";

pub fn path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        // Read once, like everything else here — a log that moved halfway
        // through a run would split one session across two files.
        if let Some(set) = std::env::var_os(PATH_OVERRIDE) {
            if !set.is_empty() {
                return PathBuf::from(set);
            }
        }
        if cfg!(target_os = "macos") {
            if let Some(home) = dirs::home_dir() {
                return home.join("Library/Logs/splaude.log");
            }
        }
        if cfg!(target_os = "windows") {
            if let Some(local) = dirs::data_local_dir() {
                return local.join("splaude").join("splaude.log");
            }
        }
        // Linux: state dir is the correct home for a log, cache is the fallback.
        dirs::state_dir()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("splaude")
            .join("splaude.log")
    })
    .clone()
}

/// The open log file, or `None` while nothing has asked for one.
fn sink() -> &'static Mutex<Option<File>> {
    static SINK: OnceLock<Mutex<Option<File>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

/// Start writing the log to a file. Until this is called, [`log`] reaches the
/// ring and stderr and nothing else.
///
/// Opt-in, and that is the entire point. It used to be automatic, so *anything*
/// that linked this crate and logged a line appended to the user's real log —
/// including the test suite, which runs its binaries in parallel, so a
/// `cargo test` interleaved fragments of unrelated runs into the one file
/// "Reveal Log" exists to show when dictation misbehaves. Making the file
/// something a program asks for means a test cannot pollute it by forgetting to
/// opt out, which is the kind of discipline that lasts.
///
/// Idempotent: the second call is a no-op rather than a second handle.
pub fn to_file() {
    let Ok(mut held) = sink().lock() else {
        return;
    };
    if held.is_some() {
        return;
    }

    let target = path();
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Never fails loudly: a log that cannot open its own file must not take
    // down the thing it exists to explain. stderr and the ring still work.
    if let Ok(handle) = OpenOptions::new().create(true).append(true).open(&target) {
        *held = Some(handle);
    }
}

/// Writes one line to stderr, the recent-line ring, and the log file if one has
/// been opened with [`to_file`].
///
/// Never fails loudly: a diagnostic that panics because its own directory is
/// unwritable would take down the thing it exists to explain.
pub fn log(area: &str, message: impl AsRef<str>) {
    let line = format!(
        "{} [{}] {}",
        chrono::Local::now().format("%H:%M:%S%.3f"),
        area,
        message.as_ref()
    );

    if let Ok(mut ring) = recent().lock() {
        ring.push(line.clone());
        let overflow = ring.len().saturating_sub(RECENT_LIMIT);
        if overflow > 0 {
            ring.drain(0..overflow);
        }
    }

    eprintln!("{line}");

    // Held across the write, so two threads cannot interleave halves of their
    // lines. The old code reopened the file per line and relied on the append
    // mode alone, which orders whole writes but not the two calls `writeln!`
    // can become once a line is long enough to be split.
    if let Ok(mut held) = sink().lock() {
        if let Some(handle) = held.as_mut() {
            let _ = writeln!(handle, "{line}");
        }
    }
}

/// The last lines logged, oldest first.
pub fn recent_line() -> Vec<String> {
    recent().lock().map(|ring| ring.clone()).unwrap_or_default()
}

/// Marks a run boundary so an old log is not mistaken for the current one.
pub fn session(note: &str) {
    log("session", format!("──── {note} ────"));
}

#[cfg(test)]
mod test {
    use super::*;

    /// The guarantee this module exists to make now: logging without asking for
    /// a file writes no file. Every test in this workspace logs something
    /// eventually, and before this they all appended to the user's real log —
    /// in parallel, so the lines interleaved mid-sentence.
    ///
    /// This test is itself the evidence: it logs, and then asserts that the
    /// place a log would go was not created by doing so. It can only pass in a
    /// process where nothing called [`to_file`], which is every test binary.
    #[test]
    fn logging_without_opening_a_file_writes_no_file() {
        log("test", "this line must not reach any file");

        let held = sink().lock().expect("the sink lock is not poisoned");
        assert!(
            held.is_none(),
            "a test process opened the log file at {}",
            path().display()
        );
    }

    /// The ring is what the tray reads, and it has to keep working when there is
    /// no file at all — which is now the default rather than an error state.
    #[test]
    fn a_line_reaches_the_ring_with_no_file_open() {
        let marker = "ring-only-marker-2f7a";
        log("test", marker);
        assert!(
            recent_line().iter().any(|line| line.contains(marker)),
            "the line never reached the ring"
        );
    }

    /// The ring is a breadcrumb trail, not a second copy of the log.
    #[test]
    fn the_ring_stays_bounded() {
        for index in 0..RECENT_LIMIT * 2 {
            log("test", format!("filling {index}"));
        }
        assert!(recent_line().len() <= RECENT_LIMIT);
    }
}
