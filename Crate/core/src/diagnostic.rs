//! Append-only log, plus stderr when run from a terminal.
//!
//! A tray app has nowhere to print, and every failure mode here (permission,
//! socket, empty audio, refused injection) looks identical from the outside —
//! "nothing happened".

use std::fs::OpenOptions;
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
pub fn path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
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

/// Writes one line to the log, stderr, and the recent-line ring.
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

    let target = path();
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut handle) = OpenOptions::new().create(true).append(true).open(&target) {
        let _ = writeln!(handle, "{line}");
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
