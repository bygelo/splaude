//! splaude — push-to-talk dictation.
//!
//! Hold the hotkey, talk, release: the text lands at the cursor in whatever app
//! had focus. This is the cross-platform build; macOS also ships a native Swift
//! app (see `Source/`), which is what the released binaries are today.

mod inject;
mod take;
#[cfg(not(target_os = "linux"))]
mod tray;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tao::event::Event;
#[cfg(not(target_os = "linux"))]
use tao::event::StartCause;
use tao::event_loop::{ControlFlow, EventLoopBuilder};

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

/// Everything that wakes the loop from outside it.
///
/// The hotkey callback and the tray menu handler both fire on whatever thread
/// the platform picked, so neither touches state directly — they send one of
/// these and let the loop, which owns the take, decide what it means.
enum Wake {
    Hotkey(HotkeyEdge),
    #[cfg(not(target_os = "linux"))]
    Menu(tray::MenuId),
}

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
///
/// The `tao` loop here is not about windows — there are none. It is the
/// main-thread run loop `global-hotkey` needs: on Windows it is the pump that
/// delivers `WM_HOTKEY` to the manager's hidden window, and on macOS it is the
/// main run loop that the Carbon hotkey handler fires on and that the manager
/// insists on being built from. A listener that spun up its own thread could
/// satisfy the first and never the second, so the loop lives here instead.
///
/// It diverges: `EventLoop::run` never returns.
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

    let event_loop = EventLoopBuilder::<Wake>::with_user_event().build();
    let edge = event_loop.create_proxy();

    // On this thread, deliberately: see the doc comment above. The callback
    // runs wherever the backend emits — inside the window procedure on Windows
    // — so it does nothing but hand the edge to the loop.
    let hotkey = HotkeyListener::new(
        setting.hotkey,
        Box::new(move |what| {
            let _ = edge.send_event(Wake::Hotkey(what));
        }),
    )?;

    #[cfg(not(target_os = "linux"))]
    tray::forward(event_loop.create_proxy(), Wake::Menu);

    println!("splaude is listening.");
    println!("Hold {} to dictate. Ctrl+C to quit.", setting.hotkey);

    // One take at a time. A second key-down before the first take finished is
    // the key repeating or a stuck modifier, not a request to open two sockets.
    let mut take: Option<Take> = None;
    // Held in an `Option` only so `LoopDestroyed` can drop it: `run` never
    // returns, so the unregister has to happen inside the loop or not at all.
    let mut hotkey = Some(hotkey);
    // Built on the loop's first tick rather than here — see `StartCause::Init`
    // below. `None` means the tray could not be created, which is survivable:
    // the hotkey still works, there is just nothing to look at or quit from.
    #[cfg(not(target_os = "linux"))]
    let mut status: Option<tray::Tray> = None;

    event_loop.run(move |event, _target, flow| {
        // Nothing here polls, so idling costs nothing — every edge arrives as a
        // user event that wakes the loop by itself.
        *flow = ControlFlow::Wait;

        match event {
            // macOS wants the status item created on a run loop that is
            // already going, not on one that has merely been built; this is
            // the first tick of it.
            #[cfg(not(target_os = "linux"))]
            Event::NewEvents(StartCause::Init) => {
                match tray::Tray::new(&setting.hotkey.to_string()) {
                    Ok(built) => status = Some(built),
                    Err(error) => eprintln!("splaude: {error:#}"),
                }
            }

            Event::UserEvent(Wake::Hotkey(HotkeyEdge::Pressed)) => {
                if take.is_none() {
                    match Take::start(
                        runtime.handle(),
                        Arc::clone(&store),
                        &setting,
                        injector.clone(),
                    ) {
                        Ok(started) => {
                            take = Some(started);
                            // Only once the take is actually live: an icon that
                            // goes red on a take that failed to start is a lie.
                            #[cfg(not(target_os = "linux"))]
                            if let Some(shown) = status.as_mut() {
                                shown.set_mood(tray::Mood::Recording);
                            }
                        }
                        Err(error) => eprintln!("splaude: {error:#}"),
                    }
                }
            }
            Event::UserEvent(Wake::Hotkey(HotkeyEdge::Released)) => {
                if let Some(running) = take.take() {
                    running.finish();
                }
                #[cfg(not(target_os = "linux"))]
                if let Some(shown) = status.as_mut() {
                    shown.set_mood(tray::Mood::Idle);
                }
            }

            // The only thing in the process that asks the loop to end, and so
            // the only reason `LoopDestroyed` below ever runs. Without it the
            // hotkey registration outlives the process's own shutdown path.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Menu(id)) => {
                if tray::is_quit(&id) {
                    *flow = ControlFlow::Exit;
                }
            }

            Event::LoopDestroyed => {
                if let Some(running) = take.take() {
                    running.finish();
                }
                drop(hotkey.take());
                // Dropping the last handle is what removes the icon; leaving it
                // to process teardown leaves a ghost in the tray until hover.
                #[cfg(not(target_os = "linux"))]
                drop(status.take());
            }
            _ => {}
        }
    })
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
