//! splaude — push-to-talk dictation.
//!
//! Hold the hotkey, talk, release: the text lands at the cursor in whatever app
//! had focus. This is the cross-platform build; macOS also ships a native Swift
//! app (see `Source/`), which is what the released binaries are today.

mod inject;
mod take;

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use splaude_core::credential::Store;
use splaude_core::{diagnostic, Setting};
use splaude_platform::{Capability, HotkeyEdge, HotkeyListener};
use take::Take;

const USAGE: &str = "\
splaude — push-to-talk dictation

USAGE:
    splaude [OPTION]

OPTIONS:
    (none)      Run. Hold the hotkey to dictate.
    --check     Report credential, permission and device state, then exit.
    --help      Show this message.
    --version   Show the version.
";

fn main() -> Result<()> {
    let argument: Vec<String> = std::env::args().skip(1).collect();

    match argument.first().map(String::as_str) {
        None => run(),
        Some("--check") => check(),
        Some("--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("--version") => {
            println!("splaude {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("splaude: unknown option {other}\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// The dictation loop.
fn run() -> Result<()> {
    diagnostic::session("start");

    let setting = Setting::load();
    let store = Arc::new(credential_store());
    let capability = Capability::detect();

    if let Some(note) = &capability.note {
        eprintln!("splaude: {note}\n");
    }
    if !capability.can_dictate() {
        bail!("this session cannot register a global hotkey or type into other windows");
    }

    // Fail before the hotkey is live rather than at the moment the user first
    // holds it — a take that dies on key-down looks like the app is broken.
    if let Err(error) = store.load() {
        eprintln!("splaude: {error}\n");
    }

    let runtime = tokio::runtime::Runtime::new().context("could not start the async runtime")?;
    let injector = inject::spawn()?;

    let (edge, inbox) = std::sync::mpsc::channel();
    let _hotkey = HotkeyListener::new(
        setting.hotkey,
        Box::new(move |what| {
            let _ = edge.send(what);
        }),
    )?;

    println!("splaude is listening.");
    println!("Hold {} to dictate. Ctrl+C to quit.", setting.hotkey);

    // One take at a time. A second key-down before the first take finished is
    // the key repeating or a stuck modifier, not a request to open two sockets.
    let mut take: Option<Take> = None;

    for what in inbox {
        match what {
            HotkeyEdge::Pressed if take.is_none() => {
                match Take::start(
                    runtime.handle(),
                    Arc::clone(&store),
                    &setting,
                    injector.clone(),
                ) {
                    Ok(started) => take = Some(started),
                    Err(error) => eprintln!("splaude: {error:#}"),
                }
            }
            HotkeyEdge::Pressed => {}
            HotkeyEdge::Released => {
                if let Some(running) = take.take() {
                    running.finish();
                }
            }
        }
    }

    Ok(())
}

/// Headless diagnostic. Opens no window and starts no take, so it is safe to
/// run over SSH or in CI — where it reports what is missing and still exits 0.
fn check() -> Result<()> {
    println!("splaude {}", env!("CARGO_PKG_VERSION"));
    println!();

    println!("credential");
    for line in credential_store().describe().lines() {
        println!("  {line}");
    }
    println!();

    let capability = Capability::detect();
    println!("capability");
    println!("  global hotkey  {}", mark(capability.global_hotkey));
    println!("  type into apps {}", mark(capability.inject_text));
    println!("  inspect focus  {}", mark(capability.inspect_focus));
    println!("  launch at login {}", mark(capability.autostart));
    if let Some(note) = &capability.note {
        println!("  note: {note}");
    }
    println!();

    let setting = Setting::load();
    println!("setting");
    println!("  hotkey    {}", setting.hotkey);
    println!("  language  {}", setting.language);
    println!("  live typing {}", mark(setting.live_typing));
    println!("  file      {}", Setting::path().display());
    println!("  log       {}", diagnostic::path().display());

    Ok(())
}

/// Where the Claude Code credential is read from.
///
/// The file is the whole story on Windows and Linux — it is where Claude Code
/// keeps the token there. macOS additionally has it in the Keychain, which this
/// build cannot read yet; the file fallback covers most installs, and the Swift
/// app is what macOS users should run today regardless.
fn credential_store() -> Store {
    Store::file_only()
}

fn mark(yes: bool) -> &'static str {
    if yes {
        "yes"
    } else {
        "no"
    }
}
