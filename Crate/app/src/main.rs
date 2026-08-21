//! splaude — push-to-talk dictation.
//!
//! Hold the hotkey, talk, release: the text lands at the cursor in whatever app
//! had focus. This is the cross-platform build; macOS also ships a native Swift
//! app (see `Source/`), which is what the released binaries are today.

/// Gated with `tray`, its only in-crate caller — Linux has no tray, so on Linux
/// every item here would be dead code. `build.rs` reaches the same source by
/// `include!` rather than through this module, so the `.ico` it renders and the
/// tray image are the same drawing.
#[cfg(not(target_os = "linux"))]
mod icon;
mod inject;
mod take;
#[cfg(not(target_os = "linux"))]
mod tray;

#[cfg(not(target_os = "linux"))]
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tao::event::Event;
#[cfg(not(target_os = "linux"))]
use tao::event::StartCause;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};

use splaude_core::credential::Store;
use splaude_core::{diagnostic, quota, update, Setting};
use splaude_platform::{autostart, focus, submit, Capability, HotkeyEdge, HotkeyListener};
use take::{Report, Take};

/// Longest press that still counts as a tap rather than a hold.
///
/// This is the whole of the tap-versus-hold decision, so the number matters in
/// both directions. Too low and a deliberate tap reads as a very short hold,
/// which starts a take and ends it again before a word gets out. Too high and
/// the shortest real dictations — "yes", "no", a name — end at the threshold
/// instead of at the key, leaving the microphone latched open with the user
/// convinced they stopped it.
///
/// 400 ms is `holdThreshold` from `Hotkey.swift`, which is the one version of
/// this number that has shipped and been lived with. It also sits above the
/// ~250 ms a comfortable double-tap runs at and below the ~500 ms a person
/// needs to say anything at all, so neither gesture is near the edge.
const TAP_CEILING: Duration = Duration::from_millis(400);

/// How often the credential is re-read, matching the Swift build's timer.
///
/// splaude reads the Claude Code token but never refreshes it, so a session
/// that outlives the token has to hear about it from somewhere. Polling is what
/// makes the menu warn *before* a take fails rather than at the moment the
/// hotkey is pressed; five minutes is under the credential's own ten-minute
/// warning window, so the warning is never more than one poll late.
#[cfg(not(target_os = "linux"))]
const HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

const USAGE: &str = "\
splaude — push-to-talk dictation

USAGE:
    splaude [OPTION]

OPTIONS:
    (none)      Run. Hold the hotkey to dictate, or tap it to latch on.
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
    /// Return was pressed — but not by us — while a take was on the air. From
    /// the hook in [`splaude_platform::submit`], which runs on this same
    /// thread and so must do nothing heavier than post this.
    Submit,
    #[cfg(not(target_os = "linux"))]
    Menu(tray::MenuId),
    /// A change of input level step, from the audio callback.
    #[cfg(not(target_os = "linux"))]
    Level(u8),
    /// One take's committed text, from the socket task.
    #[cfg(not(target_os = "linux"))]
    Transcript(String),
    /// A credential re-read, from the polling thread.
    #[cfg(not(target_os = "linux"))]
    Health(Option<String>),
    /// What an update check found, from the thread that made the request.
    #[cfg(not(target_os = "linux"))]
    Update(update::Reading),
}

// MARK: - Take state

/// How the take currently on the air is being kept there.
///
/// This is the overlap guard grown a second state rather than a flag beside it.
/// It used to be `Option<Take>` alone, and "is a take running" was the only
/// question the loop could ask; latching needs it to also answer "and does the
/// next press start one or stop this one", which is not a property a second
/// boolean can hold without the two disagreeing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grip {
    /// The key is still down and the take ends when it comes up. Carries when
    /// it went down, which is the only thing separating a tap from a hold.
    Held(Instant),
    /// The key was tapped and let go. The take runs until the next press.
    Latched,
}

/// What a hotkey edge means for the take the loop already has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Start,
    /// Leave the take running with the key up.
    Latch,
    Stop,
    Ignore,
}

/// Decides what an edge means.
///
/// Pure over the grip and the clock, so tap-versus-hold is testable with no
/// hotkey, no microphone and no desktop session — which is the only way it can
/// be tested at all.
///
/// The two `Ignore`s are each load-bearing:
///
/// - **Pressed while held** is the original overlap guard. Holding a registered
///   chord auto-repeats, and every repeat arrives as a fresh `Pressed`; opening
///   a second socket for each of them is what this has always refused. It also
///   keeps `Held`'s instant pinned to the *first* press, so a two-second hold
///   is measured from where it started rather than from the last repeat.
/// - **Released while latched** is the second half of a latching tap. The tap
///   that stops a latched take arrives as `Pressed` and takes the take with it;
///   its own release then finds nothing running, and must not be read as the
///   end of a hold that never began.
fn act(grip: Option<Grip>, edge: HotkeyEdge, now: Instant) -> Act {
    match (grip, edge) {
        (None, HotkeyEdge::Pressed) => Act::Start,
        (None, HotkeyEdge::Released) => Act::Ignore,

        (Some(Grip::Held(_)), HotkeyEdge::Pressed) => Act::Ignore,
        (Some(Grip::Held(since)), HotkeyEdge::Released) => {
            // Saturating: `Instant` is monotonic, but a press and its release
            // can be delivered out of order across the message queue, and a
            // negative duration would panic rather than merely be wrong.
            if now.saturating_duration_since(since) < TAP_CEILING {
                Act::Latch
            } else {
                Act::Stop
            }
        }

        (Some(Grip::Latched), HotkeyEdge::Pressed) => Act::Stop,
        (Some(Grip::Latched), HotkeyEdge::Released) => Act::Ignore,
    }
}

/// The take on the air, and everything that has to die with it.
struct Running {
    take: Take,
    grip: Grip,
    /// The Return watcher, installed for the life of this take and no longer.
    ///
    /// A low-level keyboard hook sits in the OS's input path for every
    /// keystroke on the desktop, so an idle splaude has no business holding
    /// one. `None` where the setting is off, the binding collides, or the
    /// platform has no watcher at all.
    submit: Option<submit::Watch>,
}

impl Running {
    fn finish(self) {
        let Self { take, submit, .. } = self;
        // Before the take, not after: `Take::finish` waits on the microphone
        // and the socket, and the hook must not outlive the take it belongs to
        // by however long that costs.
        //
        // Off Windows there is no hook, so `submit::Watch` is an uninhabited
        // enum, `Option<Watch>` is statically `None`, and clippy is right that
        // dropping it does nothing. Kept anyway: the ordering is the point, and
        // writing it only where it currently bites would leave the next
        // platform to grow a watcher with a silent use-after-take.
        #[allow(clippy::drop_non_drop)]
        drop(submit);
        take.finish();
    }
}

fn main() -> Result<()> {
    let argument: Vec<String> = std::env::args().skip(1).collect();

    match argument.first().map(String::as_str) {
        // Not `run()` alone: a failure out of here is returned to the runtime,
        // which prints it to stderr and exits — and a tray app started from
        // Explorer, a shortcut or launch-at-login has no stderr anyone will
        // ever read. The whole reason this module exists is that every failure
        // looks identical from outside ("nothing happened"), and a startup that
        // dies is the loudest case of it: the icon never appears, the hotkey
        // does nothing, and the log holds a `start` line with nothing after it.
        None => run().inspect_err(|error| diagnostic::log("start", format!("{error:#}"))),
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
    // The one place that opens the log file, and the first thing done here so
    // nothing this run does is missing from it. Deliberately not in `main`:
    // `--check`, `--version` and `--help` print their answer and leave, and a
    // report that silently creates a file somewhere is a report that surprises
    // whoever ran it — on a CI runner most of all.
    diagnostic::to_file();
    diagnostic::session("start");

    // `mut` only on the platforms that have a tray: the launch-at-login item
    // and Reload Settings are what write a setting back.
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let (mut setting, note) = Setting::load_checked();
    if let Some(line) = &note {
        // Loud on the way past, because everything downstream is now running
        // on defaults the user did not choose.
        diagnostic::log(
            "setting",
            format!("{line} — using defaults ({})", Setting::path().display()),
        );
    }
    // Warm the project bias in the background now, so the first take carries it
    // instead of triggering the harvest on the hotkey path.
    if setting.use_project_keyterm {
        splaude_core::project::warm(setting.catalog_path.as_deref());
    }

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

    // The setting is the intent and the machine is reconciled to it, not the
    // other way round — see `splaude_core::setting`. Doing it on every launch
    // is also what repairs an entry left pointing at an install location that
    // has since moved, which would otherwise launch nothing at login.
    if setting.launch_at_login != autostart::is_enabled() {
        if let Err(error) = autostart::set(setting.launch_at_login) {
            eprintln!("splaude: could not reconcile launch at login: {error:#}\n");
        }
    }

    let runtime = tokio::runtime::Runtime::new().context("could not start the async runtime")?;
    let injector = inject::spawn(setting.hotkey)?;

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

    // Sampled once here so the menu is right the moment it first opens, then
    // on a timer. The read is cached inside the store, so this costs nothing
    // on top of the `store.load()` above.
    #[cfg(not(target_os = "linux"))]
    let health = store.health().headline();
    #[cfg(not(target_os = "linux"))]
    let level_proxy = event_loop.create_proxy();
    #[cfg(not(target_os = "linux"))]
    let transcript_proxy = event_loop.create_proxy();

    #[cfg(not(target_os = "linux"))]
    {
        // Off the main thread on purpose: a credential read can reach a secret
        // store, which blocks whoever asks, and the main thread here is the
        // one drawing the menu. The loop ends when the proxy stops accepting,
        // which is how the process gets to exit.
        let proxy = event_loop.create_proxy();
        let watched = Arc::clone(&store);
        std::thread::spawn(move || loop {
            std::thread::sleep(HEALTH_INTERVAL);
            if proxy
                .send_event(Wake::Health(watched.health().headline()))
                .is_err()
            {
                break;
            }
        });
    }

    // Once at startup, and then only when asked. A manual-only check is theatre:
    // the reason people run an old build is not that downloading is hard, it is
    // that nobody told them. Once per launch also stays far inside GitHub's
    // unauthenticated allowance of sixty requests an hour, which this shares
    // with everything else on the address.
    #[cfg(not(target_os = "linux"))]
    check_update(event_loop.create_proxy());

    // Settled here rather than per take, and re-settled by a reload rather than
    // on every key-down: asking again per dictation would put the same line in
    // the log once per take.
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let mut stop_on_return = watching_return(&setting);

    let submit_proxy = event_loop.create_proxy();

    // The tray renders this and the click handler reads it, so it lives here
    // and is mirrored onto the tray rather than the other way round: asking a
    // platform menu what it currently says, to decide what a click means, is a
    // question with a different answer on every toolkit.
    #[cfg(not(target_os = "linux"))]
    let mut update_reading = update::Reading::Unknown;
    #[cfg(not(target_os = "linux"))]
    let update_proxy = event_loop.create_proxy();

    println!("splaude is listening.");
    println!(
        "Hold {} to talk, or tap to latch it on. Ctrl+C to quit.",
        setting.hotkey
    );

    // One take at a time. A second key-down before the first take finished is
    // the key repeating or a stuck modifier, not a request to open two sockets
    // — see `act`, which is where that rule now lives.
    let mut take: Option<Running> = None;
    // Held in an `Option` only so `LoopDestroyed` can drop it: `run` never
    // returns, so the unregister has to happen inside the loop or not at all.
    let mut hotkey = Some(hotkey);
    // Built on the loop's first tick rather than here — see `StartCause::Init`
    // below. `None` means the tray could not be created, which is survivable:
    // the hotkey still works, there is just nothing to look at or quit from.
    #[cfg(not(target_os = "linux"))]
    let mut status: Option<tray::Tray> = None;
    // Whether the file on disk failed to parse. While it did, nothing here
    // writes over it: the broken text is the user's own edit, and saving
    // defaults on top of it would destroy the keyterms and the binding they
    // were trying to change — the exact loss this whole item exists to prevent.
    #[cfg(not(target_os = "linux"))]
    let mut file_broken = note.is_some();

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
                match tray::Tray::new(
                    &setting.hotkey.to_string(),
                    health.clone(),
                    setting.launch_at_login,
                ) {
                    Ok(mut built) => {
                        // The complaint about the settings file, if there was
                        // one, has to survive to the first menu the user opens
                        // — the log line above is written before there is
                        // anywhere to show it.
                        built.set_note(note.clone());
                        status = Some(built);
                    }
                    Err(error) => eprintln!("splaude: {error:#}"),
                }
                if let Some(shown) = status.as_mut() {
                    shown.set_project(project_name(&setting), setting.use_project_keyterm);
                }
            }

            Event::UserEvent(Wake::Hotkey(edge)) => {
                let now = Instant::now();
                match act(take.as_ref().map(|running| running.grip), edge, now) {
                    Act::Start => {
                        #[cfg(not(target_os = "linux"))]
                        let report = report(&level_proxy, &transcript_proxy);
                        #[cfg(target_os = "linux")]
                        let report = Report::silent();

                        match Take::start(
                            runtime.handle(),
                            Arc::clone(&store),
                            &setting,
                            injector.clone(),
                            report,
                        ) {
                            Ok(started) => {
                                take = Some(Running {
                                    take: started,
                                    // Every take begins as a hold. A tap is not
                                    // something that can be recognised at
                                    // key-down — it is a hold that turned out
                                    // to be short — so latching is decided at
                                    // the release and never here.
                                    grip: Grip::Held(now),
                                    submit: watch_return(stop_on_return, &submit_proxy),
                                });
                                // Only once the take is actually live: an icon that
                                // goes red on a take that failed to start is a lie.
                                // Step zero — the meter starts empty and the first
                                // audio buffer raises it.
                                #[cfg(not(target_os = "linux"))]
                                if let Some(shown) = status.as_mut() {
                                    shown.set_mood(tray::Mood::Recording(0));
                                }
                            }
                            Err(error) => {
                                eprintln!("splaude: {error:#}");
                                // The take may well have died on the credential, so
                                // say so in the menu now rather than at the next
                                // poll, five minutes from now.
                                #[cfg(not(target_os = "linux"))]
                                if let Some(shown) = status.as_mut() {
                                    shown.set_health(store.health().headline());
                                }
                            }
                        }
                    }

                    // The key is up and the take stays on the air. Nothing to
                    // tear down and nothing for the icon to say — it is already
                    // red, and it is still recording.
                    Act::Latch => {
                        if let Some(running) = take.as_mut() {
                            running.grip = Grip::Latched;
                            diagnostic::log("take", "tapped — latched until the next press");
                        }
                    }

                    Act::Stop => {
                        if let Some(running) = take.take() {
                            running.finish();
                        }
                        #[cfg(not(target_os = "linux"))]
                        if let Some(shown) = status.as_mut() {
                            shown.set_mood(tray::Mood::Idle);
                            // The harvest ran for this take, so the resolved
                            // project is fresh and free to read here.
                            shown.set_project(project_name(&setting), setting.use_project_keyterm);
                        }
                    }

                    Act::Ignore => {}
                }
            }

            // Submitting is a statement that you are done talking. The
            // keystroke itself was never consumed — see `splaude_platform
            // ::submit` — so whatever the user pressed Return in has already
            // sent; this only ends the dictation.
            Event::UserEvent(Wake::Submit) => {
                if let Some(running) = take.take() {
                    diagnostic::log("submit", "Return pressed — ending the take");
                    running.finish();
                    #[cfg(not(target_os = "linux"))]
                    if let Some(shown) = status.as_mut() {
                        shown.set_mood(tray::Mood::Idle);
                    }
                }
            }

            // Only while a take is on the air: the audio thread is still
            // draining when the key comes up, and a level that arrived after
            // that would re-redden an icon which has already gone idle.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Level(step)) => {
                if let Some(shown) = status.as_mut() {
                    if shown.is_recording() {
                        shown.set_mood(tray::Mood::Recording(step));
                    }
                }
            }

            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Transcript(text)) => {
                if let Some(shown) = status.as_mut() {
                    shown.set_transcript(&text);
                }
            }

            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Health(headline)) => {
                if let Some(shown) = status.as_mut() {
                    shown.set_health(headline);
                }
            }

            // Kept here as well as on the tray because the click needs it: the
            // tray renders the reading, and this decides what clicking it does.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Update(reading)) => {
                update_reading = reading.clone();
                if let Some(shown) = status.as_mut() {
                    shown.set_update(reading);
                }
            }
            // The only thing in the process that asks the loop to end, and so
            // the only reason `LoopDestroyed` below ever runs. Without it the
            // hotkey registration outlives the process's own shutdown path.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Menu(id)) => match tray::ask(&id) {
                tray::Ask::Quit => *flow = ControlFlow::Exit,

                tray::Ask::CopyTranscript => {
                    if let Some(shown) = status.as_ref() {
                        shown.copy_transcript();
                    }
                }

                tray::Ask::RevealLog => tray::reveal_log(),

                // Deliberately not gated on whether a take is running: the
                // whole point is to answer "do keystrokes from this app land
                // anywhere" without speaking, so it has to work at rest.
                tray::Ask::TestPaste => take::probe(setting.clone(), injector.clone()),

                // One item, two meanings, decided by what the last check found:
                // an update that exists is a link, and anything else is an
                // invitation to look again. Re-checking on a click is also the
                // only way back from a failed check without restarting.
                tray::Ask::Update => match &update_reading {
                    update::Reading::Available(release) => tray::open_release(&release.url),
                    _ => check_update(update_proxy.clone()),
                },

                tray::Ask::EditSetting => tray::edit_setting(&setting),

                // Refused mid-take rather than deferred. A deferred reload has
                // to be remembered and then fired from wherever the take
                // happens to end — release, latch-stop, Return, or Quit — and
                // every one of those paths would have to be right for the
                // hotkey not to move underneath a user who is still holding it.
                // Refusing is one path, it is honest about what happened, and
                // the recovery is to click the item again a second later.
                tray::Ask::ReloadSetting => {
                    if take.is_some() {
                        diagnostic::log(
                            "setting",
                            "a take is on the air — not reloading; try again once it ends",
                        );
                    } else {
                        file_broken = !reload(
                            &mut setting,
                            hotkey.as_mut(),
                            &injector,
                            &mut stop_on_return,
                            status.as_mut(),
                        );
                    }
                }

                tray::Ask::ToggleAutostart => {
                    let want = !setting.launch_at_login;
                    match autostart::set(want) {
                        Ok(()) => {
                            setting.launch_at_login = want;
                            if file_broken {
                                diagnostic::log(
                                    "setting",
                                    "the file did not parse, so it is left exactly as written \
                                     — launch at login is set on the machine only",
                                );
                            } else if let Err(error) = setting.save() {
                                eprintln!("splaude: could not save the setting: {error}");
                            }
                        }
                        Err(error) => eprintln!("splaude: {error:#}"),
                    }
                    // Redrawn from the machine rather than from the intent: a
                    // checked box over an entry that was never written is a
                    // promise the app will not keep at the next login.
                    if let Some(shown) = status.as_mut() {
                        shown.set_autostart(autostart::is_enabled());
                    }
                }

                tray::Ask::ToggleProjectKeyterm => {
                    setting.use_project_keyterm = !setting.use_project_keyterm;
                    if let Err(error) = setting.save() {
                        eprintln!("splaude: could not save the setting: {error}");
                    }
                    if let Some(shown) = status.as_mut() {
                        shown.set_project(project_name(&setting), setting.use_project_keyterm);
                    }
                }

                tray::Ask::Ignore => {}
            },

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

/// Whether a take should install the Return watcher at all.
///
/// Three inputs, and only one of them is the user's preference: the platform
/// has to have a watcher, and the binding must not be Return itself.
fn watching_return(setting: &Setting) -> bool {
    if !setting.stop_on_return || !submit::is_supported() {
        return false;
    }

    if submit::collides(setting.hotkey) {
        // Not a failure — the watcher stands down and everything else works —
        // but the user asked for a behaviour they are not getting, and a
        // silently absent one is the thing this app logs against.
        diagnostic::log(
            "submit",
            format!(
                "{} is itself the key that ends a take, so Return cannot also stop one \
                 while it is bound. Pick another hotkey, or turn stopOnReturn off.",
                setting.hotkey
            ),
        );
        return false;
    }

    true
}

/// Re-reads the settings file and applies what can be applied while running.
///
/// Called only with no take on the air; see the menu arm for why that is a
/// refusal rather than a deferral.
///
/// Most of a [`Setting`] is read per take, so applying it is nothing more than
/// replacing the value the loop holds. Two things are not: the hotkey is
/// registered with the OS and has to be moved through the listener, and
/// launch-at-login is a fact about the machine that the file is only the intent
/// for — the same reconcile `run` does at startup.
///
/// Answers whether the file parsed, which is what decides if anything is
/// allowed to write over it afterwards.
#[cfg(not(target_os = "linux"))]
#[must_use]
fn reload(
    setting: &mut Setting,
    hotkey: Option<&mut HotkeyListener>,
    injector: &Sender<inject::Command>,
    stop_on_return: &mut bool,
    mut status: Option<&mut tray::Tray>,
) -> bool {
    let (mut next, note) = Setting::load_checked();

    if let Some(note) = note {
        // The one place where falling back to defaults would be actively
        // destructive: this process already holds a working setting, and
        // replacing it over a mistyped comma is precisely the silent revert
        // this item exists to make visible. Keep what is running.
        diagnostic::log("setting", format!("{note} — keeping the settings in use"));
        if let Some(shown) = status.as_mut() {
            shown.set_note(Some(note));
        }
        return false;
    }

    if next.hotkey != setting.hotkey {
        match hotkey {
            Some(listener) => match listener.rebind(next.hotkey) {
                Ok(()) => {
                    diagnostic::log("setting", format!("hotkey is now {}", next.hotkey));
                    // The injector has to hear about it too: it leaves the live
                    // binding's modifier alone and clears every other one, so a
                    // rebind it never saw would guard the wrong key.
                    let _ = injector.send(inject::Command::Bind {
                        binding: next.hotkey,
                    });
                }
                Err(error) => {
                    // `rebind` restores the previous registration on refusal,
                    // so the user still has a working hotkey. What must not
                    // happen is the in-memory setting drifting away from what
                    // is actually registered — the injector and the Return
                    // watcher both key off it.
                    next.hotkey = listener.binding();
                    diagnostic::log(
                        "setting",
                        format!("{error:#} — still bound to {}", next.hotkey),
                    );
                }
            },
            None => {
                next.hotkey = setting.hotkey;
                diagnostic::log("setting", "no hotkey listener — the binding is unchanged");
            }
        }
    }

    if next.launch_at_login != setting.launch_at_login {
        if let Err(error) = autostart::set(next.launch_at_login) {
            diagnostic::log(
                "setting",
                format!("could not reconcile launch at login: {error:#}"),
            );
            // Same rule as the hotkey: the value we keep is the one the machine
            // actually has, not the one the file asked for.
            next.launch_at_login = autostart::is_enabled();
        }
    }

    let changed = setting.difference(&next);
    *setting = next;
    *stop_on_return = watching_return(setting);

    if changed.is_empty() {
        diagnostic::log("setting", "reloaded — nothing changed");
    } else {
        diagnostic::log("setting", format!("reloaded: {}", changed.join(", ")));
    }

    if let Some(shown) = status {
        shown.set_note(None);
        shown.set_hotkey(&setting.hotkey.to_string());
        shown.set_autostart(autostart::is_enabled());
        // Nothing above necessarily changed anything the menu draws, and a
        // reload that leaves a stale menu behind is a reload the user cannot
        // see happened.
        shown.refresh();
    }

    true
}

/// Installs the Return watcher for one take, if this build is watching at all.
///
/// The hook goes in **on this thread** because that is where it has to live.
/// `WH_KEYBOARD_LL` is delivered by posting to the message queue of the thread
/// that installed it, so a hook installed anywhere without a pump is silently
/// never called — and the only thread in this process guaranteed to be pumping
/// is this one, the `tao` loop `global-hotkey` already requires. That is also
/// why this is a free function called from inside the loop rather than
/// something `Take::start` does: a take may not care which thread it is on,
/// but this does.
///
/// The callback fires from inside the hook procedure, in the OS's input path
/// for every keystroke on the desktop, so it does nothing but post — the same
/// shape as the hotkey and tray callbacks, for a stricter version of the same
/// reason.
/// Ask GitHub what the newest release is, on a thread that may block.
///
/// Fire and forget. The answer arrives as a [`Wake::Update`] if the loop is
/// still running and is dropped if it is not, which is what should happen to an
/// update notice for a process that is quitting. Nothing waits on this and
/// nothing fails if it never answers — an unreachable network leaves the menu
/// item exactly where it was.
#[cfg(not(target_os = "linux"))]
fn check_update(proxy: EventLoopProxy<Wake>) {
    std::thread::spawn(move || {
        let reading = update::check();
        diagnostic::log("update", reading.line());
        let _ = proxy.send_event(Wake::Update(reading));
    });
}

fn watch_return(watching: bool, proxy: &EventLoopProxy<Wake>) -> Option<submit::Watch> {
    if !watching {
        return None;
    }

    let proxy = proxy.clone();
    match submit::watch(Box::new(move || {
        let _ = proxy.send_event(Wake::Submit);
    })) {
        Ok(watch) => watch,
        Err(error) => {
            // A take with no watcher is a take that behaves the way it did
            // before this existed, which is not worth refusing to record over.
            diagnostic::log("submit", format!("{error:#}"));
            None
        }
    }
}

/// The callbacks for one take, wired to the tray through the event loop.
///
/// Fresh per take because the level filter carries state: the last step it
/// pushed, which has to start from "nothing yet" so the first buffer of a new
/// take always lands.
#[cfg(not(target_os = "linux"))]
fn report(
    level: &tao::event_loop::EventLoopProxy<Wake>,
    transcript: &tao::event_loop::EventLoopProxy<Wake>,
) -> Report {
    use std::sync::atomic::{AtomicU8, Ordering};

    let level = level.clone();
    let transcript = transcript.clone();
    // Atomic rather than a `Cell` because the audio callback hands out `Fn`,
    // not `FnMut`, and it runs on a thread neither the loop nor this function
    // owns. `u8::MAX` is not a step, so the first buffer always reports.
    let last = AtomicU8::new(u8::MAX);

    Report {
        level: Box::new(move |reading| {
            // The whole point of the filter: the audio thread calls this
            // hundreds of times a second and each step costs a rasterised
            // 32x32 bitmap plus a platform icon swap, so only a change of step
            // is worth waking the loop for.
            let step = tray::step_of(reading);
            if last.swap(step, Ordering::Relaxed) != step {
                let _ = level.send_event(Wake::Level(step));
            }
        }),
        transcript: Box::new(move |text| {
            let _ = transcript.send_event(Wake::Transcript(text));
        }),
    }
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

    // The claim this whole report exists to back up: dictation is not supposed
    // to spend Claude quota, and the rate-limit headers on the speech socket's
    // handshake are the only client-side evidence either way. This process has
    // opened no socket, so the honest answer here is always that nobody has
    // asked yet — which is why the never-dictated state is a distinct reading
    // rather than a cheerful "none seen".
    println!("quota");
    println!("  rate limit  {}", quota::summary());
    println!("  a handshake that answers with no anthropic-ratelimit header spent nothing");
    println!();

    // The one thing in this report that reaches the network, and the only
    // reason `--check` is not entirely offline. Worth the exception: someone
    // running this because dictation misbehaved should be told if they are
    // three releases behind before they read anything else here.
    println!("update");
    println!("  {}", update::check().line());
    println!();

    let (setting, note) = Setting::load_checked();
    println!("setting");
    if let Some(line) = &note {
        println!("  {line}");
        println!("  everything below is the default, not what the file says");
    }
    println!("  hotkey    {}", setting.hotkey);
    println!("  language  {}", setting.language);
    println!("  live typing {}", mark(setting.live_typing));
    // Names whatever is in front right now, and says whether a take aimed at it
    // would be buffered. A user whose dictation arrives as a run of `a` in a
    // remote-desktop window needs a way to see that from outside the log.
    match focus::executable() {
        Some(name) if setting.is_keycode_app(&name) => {
            println!("  foreground {name} — buffered, it re-encodes keystrokes by keycode")
        }
        Some(name) => println!("  foreground {name}"),
        None => println!("  foreground  (this platform cannot name it)"),
    }
    println!("  file      {}", Setting::path().display());
    println!("  log       {}", diagnostic::path().display());
    println!();

    // The whole point of project bias is that the user never configures it, so
    // this is the only place they can see what it decided. A wrong project or a
    // wrong term is otherwise invisible until a dictation comes back mangled.
    println!("recogniser bias");
    match splaude_core::project::active() {
        Some(project) if setting.use_project_keyterm => {
            println!("  project   {} ({})", project.name, project.root.display());
        }
        Some(project) => println!("  project   {} — off in the setting", project.name),
        None => println!("  project   none — no recent Claude Code session"),
    }
    match splaude_core::project::catalog_keyterm(setting.catalog_path.as_deref()) {
        found if found.is_empty() => println!("  catalog   none found"),
        found => println!("  catalog   {} name", found.len()),
    }
    println!(
        "  recent    {}",
        splaude_core::project::recent_name(8).join(", ")
    );
    let packed = splaude_core::speech::anthropic::pack_keyterm(&setting.wire_keyterm_sync());
    println!("  budget    {} of 1024 characters", packed.len());
    println!("  keyterm   {packed}");

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

/// The project name shown in the tray, or `None` when bias is off or nothing
/// resolved.
///
/// Resolution is skipped entirely when the setting is off: reading someone's
/// session log to label a menu item they have switched off is work they did not
/// ask for.
fn project_name(setting: &Setting) -> Option<String> {
    if !setting.use_project_keyterm {
        return None;
    }
    splaude_core::project::active().map(|project| project.name)
}

#[cfg(test)]
mod test {
    use super::*;

    // Classification only. Nothing here starts a take, installs a hook, opens
    // a device or synthesises a keystroke — `act` is pure precisely so the
    // gesture that decides whether the microphone stays open can be checked
    // with none of them.

    /// A press that started `ago` in the past.
    fn held(ago: Duration) -> (Option<Grip>, Instant) {
        let now = Instant::now();
        let since = now
            .checked_sub(ago)
            .expect("the monotonic clock should be older than the test");
        (Some(Grip::Held(since)), now)
    }

    /// Comfortably inside the tap window, and comfortably outside it.
    const BRIEF: Duration = Duration::from_millis(80);
    const LONG: Duration = Duration::from_millis(2_000);

    #[test]
    fn a_press_with_no_take_starts_one() {
        assert_eq!(act(None, HotkeyEdge::Pressed, Instant::now()), Act::Start);
    }

    #[test]
    fn holding_and_letting_go_ends_the_take() {
        // The original behaviour, and the one that must not have changed:
        // push-to-talk is still push-to-talk.
        let (grip, now) = held(LONG);
        assert_eq!(act(grip, HotkeyEdge::Released, now), Act::Stop);
    }

    #[test]
    fn tapping_latches_the_take_on() {
        let (grip, now) = held(BRIEF);
        assert_eq!(act(grip, HotkeyEdge::Released, now), Act::Latch);
    }

    #[test]
    fn the_next_press_while_latched_stops_the_take() {
        // The interaction that matters most: a second tap must end the take
        // rather than try to open a second one alongside it.
        assert_eq!(
            act(Some(Grip::Latched), HotkeyEdge::Pressed, Instant::now()),
            Act::Stop
        );
    }

    #[test]
    fn the_release_of_the_stopping_tap_does_nothing() {
        // That press already took the take with it, so by the time its own
        // release arrives there is nothing running — and a release with no take
        // must never be read as the end of a hold.
        assert_eq!(act(None, HotkeyEdge::Released, Instant::now()), Act::Ignore);
    }

    #[test]
    fn a_repeat_while_held_does_not_open_a_second_take() {
        // Holding a registered chord auto-repeats, and every repeat arrives as
        // a fresh press. This is the overlap guard the loop has always had.
        let (grip, now) = held(LONG);
        assert_eq!(act(grip, HotkeyEdge::Pressed, now), Act::Ignore);
    }

    #[test]
    fn a_repeat_does_not_reset_when_the_hold_began() {
        // The corollary of the rule above: because the repeat is ignored, the
        // grip keeps the *first* press's instant, so a long hold is still
        // measured from where it started. If a repeat re-armed the timer, every
        // hold would look like a tap and latch instead of ending.
        let (grip, now) = held(LONG);
        assert_eq!(act(grip, HotkeyEdge::Pressed, now), Act::Ignore);
        assert_eq!(act(grip, HotkeyEdge::Released, now), Act::Stop);
    }

    #[test]
    fn a_latched_take_ignores_a_release() {
        assert_eq!(
            act(Some(Grip::Latched), HotkeyEdge::Released, Instant::now()),
            Act::Ignore
        );
    }

    #[test]
    fn the_threshold_is_the_only_thing_separating_the_two_gesture() {
        // Either side of the line, to the millisecond. Exactly at the ceiling
        // is a hold: a tap is a press *shorter* than the threshold, so the
        // boundary belongs to the gesture that keeps the old behaviour.
        let (just_under, now) = held(TAP_CEILING - Duration::from_millis(1));
        assert_eq!(act(just_under, HotkeyEdge::Released, now), Act::Latch);

        let (exactly, now) = held(TAP_CEILING);
        assert_eq!(act(exactly, HotkeyEdge::Released, now), Act::Stop);

        let (just_over, now) = held(TAP_CEILING + Duration::from_millis(1));
        assert_eq!(act(just_over, HotkeyEdge::Released, now), Act::Stop);
    }

    #[test]
    fn a_release_that_arrives_before_its_press_is_a_hold_not_a_panic() {
        // Both edges cross a message queue, and nothing guarantees the clock
        // read for one lands after the clock read for the other. Subtracting
        // the wrong way round would panic in the middle of a take.
        let now = Instant::now();
        let future = now + Duration::from_secs(1);
        assert_eq!(
            act(Some(Grip::Held(future)), HotkeyEdge::Released, now),
            Act::Latch
        );
    }

    #[test]
    fn the_tap_window_is_a_gesture_not_an_accident() {
        // Guards the constant itself. Below a tenth of a second no human taps
        // reliably; above a second the shortest real dictations would latch.
        assert!(TAP_CEILING >= Duration::from_millis(100));
        assert!(TAP_CEILING <= Duration::from_millis(1_000));
    }

    #[test]
    fn every_edge_has_an_answer_in_every_grip() {
        // The state machine is total: there is no combination of grip and edge
        // that falls through to a default, because a missed one would leave the
        // microphone open with no way to close it.
        let now = Instant::now();
        for grip in [None, Some(Grip::Held(now)), Some(Grip::Latched)] {
            for edge in [HotkeyEdge::Pressed, HotkeyEdge::Released] {
                let _ = act(grip, edge, now);
            }
        }
    }
}
