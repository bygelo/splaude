//! splaude — push-to-talk dictation.
//!
//! Hold the hotkey, talk, release: the text lands at the cursor in whatever app
//! had focus. This is the cross-platform build; macOS also ships a native Swift
//! app (see `Source/`), which is what the released binaries are today.

#[cfg(not(target_os = "linux"))]
mod floating;
/// Gated with `tray`, its only in-crate caller — Linux has no tray, so on Linux
/// every item here would be dead code. `build.rs` reaches the same source by
/// `include!` rather than through this module, so the `.ico` it renders and the
/// tray image are the same drawing.
#[cfg(not(target_os = "linux"))]
mod icon;
mod inject;
#[cfg(not(target_os = "linux"))]
mod setting_window;
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
use splaude_platform::{autostart, control, focus, submit, Capability, HotkeyEdge, HotkeyListener};
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

/// Below this share of full scale, nobody is talking.
///
/// Deliberately far under the meter's first step, which is a fifth of full
/// scale: this is a noise floor, and a threshold set where the meter's steps
/// are would call an ordinary quiet voice silence and end their take
/// mid-sentence. Speech in the log of a real take peaks around 0.5.
#[cfg(not(target_os = "linux"))]
const SILENCE_LEVEL: f32 = 0.02;

/// What a user with an expired or missing credential has to run.
///
/// splaude reads the credential Claude Code already wrote and never refreshes
/// it, so the fix is always to make Claude Code write a fresh one. Same string
/// the Swift build puts on the pasteboard.
#[cfg(not(target_os = "linux"))]
const CREDENTIAL_FIX: &str = "claude";

const USAGE: &str = "\
splaude — push-to-talk dictation

USAGE:
    splaude [OPTION]

OPTIONS:
    (none)      Run. Hold the hotkey to dictate, or tap it to latch on.
    --check     Report credential, permission and device state, then exit.
    --settings  Open the settings window on its own, without dictating.
    --toggle    Start or stop a take in the running splaude, then exit.
    --cancel    Discard the take in progress, then exit.
    --help      Show this message.
    --version   Show the version.
";

/// Everything that wakes the loop from outside it.
///
/// The hotkey callback and the tray menu handler both fire on whatever thread
/// the platform picked, so neither touches state directly — they send one of
/// these and let the loop, which owns the take, decide what it means.
enum Wake {
    /// A hotkey edge, and the instant it actually happened.
    ///
    /// The instant is carried rather than read when the loop gets to it, and
    /// that is the whole of tap-versus-hold being correct. `Take::start` opens
    /// a WebSocket on this thread, which takes a few hundred milliseconds
    /// against a live endpoint — so the release of a genuine tap is not looked
    /// at until well after [`TAP_CEILING`] has passed, and timing it on arrival
    /// reads every tap during a slow handshake as a hold that ended. The take
    /// then stops the instant it started, which from outside is a tap that
    /// records nothing.
    Hotkey(HotkeyEdge, Instant),
    /// Return was pressed — but not by us — while a take was on the air. From
    /// the hook in [`splaude_platform::submit`], which runs on this same
    /// thread and so must do nothing heavier than post this.
    Submit,
    #[cfg(not(target_os = "linux"))]
    Menu(tray::MenuId),
    /// A change of input level step, from the audio callback.
    #[cfg(not(target_os = "linux"))]
    Level(u8),
    /// The microphone crossed into or out of silence. Sent only on the
    /// transition, so it costs nothing while somebody is talking.
    #[cfg(not(target_os = "linux"))]
    Silence(bool),
    /// One take's committed text, from the socket task.
    #[cfg(not(target_os = "linux"))]
    Transcript(String),
    /// A credential re-read, from the polling thread.
    #[cfg(not(target_os = "linux"))]
    Health(Option<String>),
    /// What an update check found, from the thread that made the request.
    #[cfg(not(target_os = "linux"))]
    Update(update::Reading),
    /// Start or stop a take, asked for by something that is not the hotkey —
    /// the tray's Start Dictation, or the floating button. Kept as one event
    /// rather than two so both surfaces mean exactly the same thing.
    #[cfg(not(target_os = "linux"))]
    Gesture,
    /// The floating button was dragged, and this is where it now sits.
    #[cfg(not(target_os = "linux"))]
    FloatingMoved(splaude_core::setting::Point),
    /// A word from another splaude process — a second launch, or a `--toggle`
    /// bound to something that can reach the keyboard when the hotkey cannot.
    Control(control::Command),
    /// The settings window wrote the file. Carries nothing: the file is the
    /// message, and the loop re-reads it through the same `reload` a click on
    /// *Reload Settings* runs.
    #[cfg(not(target_os = "linux"))]
    SettingSaved,
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

/// What the latch timeout wants doing, right now.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Latch {
    /// The take has been latched and silent for long enough. End it.
    Expired,
    /// Not yet — sleep until exactly this instant rather than until the next
    /// event, because a microphone left open in a silent room produces none.
    WaitUntil(Instant),
    /// Nothing is waiting on a clock.
    Nothing,
}

/// Decides whether a latched take has been silent long enough to end itself.
///
/// Pure over the grip, the clock and the setting, for the same reason [`act`]
/// is: it is a rule with four inputs that fires seconds after the event that
/// armed it, and the only way to test it inside a real take is to sit in a
/// silent room with a microphone — which the machine this was written on does
/// not have.
///
/// A **held** take is never ended here, and that is the design rather than an
/// oversight. The key is the contract: a hold ends when the user lets go, so a
/// hold that outlived the timeout is somebody still pressing the button, not
/// somebody who forgot. Only a latched take can be forgotten, because only a
/// latched take has nothing holding it open.
#[cfg(not(target_os = "linux"))]
fn latch(
    grip: Option<Grip>,
    quiet_since: Option<Instant>,
    timeout_second: u32,
    now: Instant,
) -> Latch {
    if timeout_second == 0 {
        return Latch::Nothing;
    }

    let (Some(Grip::Latched), Some(since)) = (grip, quiet_since) else {
        return Latch::Nothing;
    };

    let timeout = Duration::from_secs(u64::from(timeout_second));

    // Saturating: the instant is stamped by the audio callback on another
    // thread, and a clock read there that lands microseconds ahead of this one
    // would otherwise panic rather than merely be early.
    if now.saturating_duration_since(since) >= timeout {
        Latch::Expired
    } else {
        Latch::WaitUntil(since + timeout)
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
    /// Ends the take and throws its words away — see [`Take::discard`].
    fn discard(self) {
        let Self { take, submit, .. } = self;
        #[allow(clippy::drop_non_drop)]
        drop(submit);
        take.discard();
    }

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
    declare_dpi_awareness();

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

        // The window on its own, with no hotkey, no microphone and no tray.
        //
        // Worth a flag of its own rather than only a menu item, because on
        // Windows the menu it lives in can be genuinely unreachable: new
        // notification-area icons are hidden behind the `^` overflow by
        // default, and a user who has not dragged splaude out of it has no way
        // to click anything. It is also what a user whose binding is being
        // swallowed needs — the thing they have to change is the binding.
        #[cfg(not(target_os = "linux"))]
        Some("--settings") => settings(),

        // Both hand a word to the instance that is already running and exit.
        // They are the answer to a hotkey that cannot be delivered — a
        // full-screen remote desktop takes the keyboard before any local app
        // sees it — so anything that can run a command can still start a take.
        Some("--toggle") => forward(control::Command::Toggle),
        Some("--cancel") => forward(control::Command::Cancel),

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

    // Before anything is registered or opened. A second splaude would fail on
    // the hotkey a moment later and exit with nothing on screen to say why —
    // which is what double-clicking the icon while it was already running used
    // to look like. Now the running copy is told to come to the front, and this
    // process leaves without touching the microphone, the tray or the binding.
    let channel = match control::claim() {
        Ok(Some(channel)) => Some(channel),
        Ok(None) => {
            diagnostic::log("start", "splaude is already running — raising it instead");
            let _ = control::send(control::Command::Show);
            println!("splaude is already running.");
            return Ok(());
        }
        // A channel that cannot be claimed for some other reason is not worth
        // refusing to start over: the cost is a second instance nobody asked
        // for, and the cost of the alternative is no dictation at all.
        Err(error) => {
            diagnostic::log(
                "start",
                format!("could not claim the control channel: {error:#} — carrying on without one"),
            );
            None
        }
    };

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
            // Stamped here, in the callback the backend fires, which is as
            // close to the physical key as this process can get.
            let _ = edge.send_event(Wake::Hotkey(what, Instant::now()));
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

    // How Start Dictation reaches the take. The menu does not open or close a
    // take itself — it posts the hotkey edges a tap would have produced, and
    // the arm above decides what they mean, so there is exactly one path that
    // starts a take and one set of rules about when it may.
    #[cfg(not(target_os = "linux"))]
    let gesture_proxy = event_loop.create_proxy();

    // Handed to each settings window so it can say it saved.
    #[cfg(not(target_os = "linux"))]
    let setting_proxy = event_loop.create_proxy();

    // Handed to the floating button, which talks to the loop the same way the
    // tray does — by posting, never by touching state.
    #[cfg(not(target_os = "linux"))]
    let floating_proxy = event_loop.create_proxy();

    // The control channel's listener blocks, so it gets a thread of its own. It
    // posts to the loop exactly like every other outside caller — the tray, the
    // button, the hotkey — so a command from another process cannot mean
    // anything a click could not already mean.
    if let Some(channel) = channel {
        let proxy = event_loop.create_proxy();
        let spawned = std::thread::Builder::new()
            .name("splaude-control".into())
            .spawn(move || {
                channel.serve(move |command| {
                    let _ = proxy.send_event(Wake::Control(command));
                });
                diagnostic::log("control", "the control channel stopped listening");
            });

        if let Err(error) = spawned {
            diagnostic::log(
                "control",
                format!("could not start the control listener: {error}"),
            );
        }
    }

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
    // `None` when the setting is off, when the button could not be created, or
    // on a platform that has none. Every use is through `wear`, so an absent
    // button is not a special case anywhere else.
    #[cfg(not(target_os = "linux"))]
    let mut floating: Option<floating::Handle> = None;
    // When the microphone last went quiet, or `None` while somebody is talking.
    // Only ever consulted for a latched take — see `latch_timeout_second`.
    #[cfg(not(target_os = "linux"))]
    let mut quiet_since: Option<Instant> = None;
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

                floating = raise_floating(&setting, &floating_proxy);
            }

            Event::UserEvent(Wake::Hotkey(edge, now)) => {
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
                                wear(status.as_mut(), floating.as_ref(), tray::Mood::Recording(0));
                            }
                            Err(error) => {
                                // Both, and the log is the half that matters.
                                // A tray app started from Explorer or from
                                // launch-at-login has no stderr anyone will
                                // read, so a take that cannot start has always
                                // looked from outside exactly like a hotkey
                                // that did nothing — which is the failure this
                                // log exists to end. "No input device" is the
                                // common one, and it is not a splaude problem
                                // to solve: a remote-desktop session with no
                                // audio redirected has no microphone at all.
                                diagnostic::log(
                                    "take",
                                    format!("could not start: {error:#}"),
                                );
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
                        wear(status.as_mut(), floating.as_ref(), tray::Mood::Idle);
                    }

                    Act::Ignore => {}
                }
            }

            // A click is a tap, and says so in edges rather than in state.
            // Starting a take from here by hand would mean a second copy of the
            // start path — the report, the grip, the Return watcher, the icon,
            // the credential re-read on failure — and two copies of "when may a
            // take begin" is how the overlap guard gets a hole in it. Posting
            // the gesture instead means a click can only ask for what a person
            // with a keyboard could already have asked for.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Gesture) => {
                let edge: &[HotkeyEdge] = match take.as_ref().map(|running| running.grip) {
                    // Press then release: `act` starts the take on the press and
                    // latches it on the release, because the two arrive
                    // microseconds apart and that is a tap. Latched is the only
                    // grip that makes sense for a click — nothing is being held
                    // down, so nothing can end the take by being let go.
                    None => &[HotkeyEdge::Pressed, HotkeyEdge::Released],
                    // A latched take ends on the next press, which is the same
                    // gesture that ends it from the keyboard.
                    Some(Grip::Latched) => &[HotkeyEdge::Pressed],
                    // The key is physically down. A release we invented would
                    // end the take underneath a user who is still holding it,
                    // and their real release would then arrive with nothing to
                    // end. Let the key finish what the key started.
                    Some(Grip::Held(_)) => {
                        diagnostic::log(
                            "take",
                            "the hotkey is held — let go of it rather than clicking",
                        );
                        &[]
                    }
                };

                // One instant for both edges, so the pair is unambiguously a
                // tap however long the socket takes to open in between.
                let at = Instant::now();
                for what in edge {
                    let _ = gesture_proxy.send_event(Wake::Hotkey(*what, at));
                }
            }

            // Written through immediately rather than at exit: the process is a
            // background app that is usually ended by Quit or by logging out,
            // and a position only saved on a clean shutdown is a position that
            // is usually lost.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::FloatingMoved(point)) => {
                setting.floating_button_point = Some(point);
                if file_broken {
                    diagnostic::log(
                        "floating",
                        "the settings file did not parse, so the new position is not saved over it",
                    );
                } else if let Err(error) = setting.save() {
                    diagnostic::log("floating", format!("could not save the position: {error}"));
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
                    wear(status.as_mut(), floating.as_ref(), tray::Mood::Idle);
                }
            }

            // Only while a take is on the air: the audio thread is still
            // draining when the key comes up, and a level that arrived after
            // that would re-redden an icon which has already gone idle.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Level(step)) => {
                let recording = status.as_ref().is_some_and(tray::Tray::is_recording);
                if recording {
                    wear(status.as_mut(), floating.as_ref(), tray::Mood::Recording(step));
                }
            }

            // Deliberately expressed in the vocabulary the tray and the
            // floating button already use, rather than in a second one. A
            // command from outside can ask for exactly what a click can ask
            // for, which is what keeps the channel from becoming a way to drive
            // this app from a script in ways nobody designed for.
            Event::UserEvent(Wake::Control(command)) => match command {
                control::Command::Show => {
                    #[cfg(not(target_os = "linux"))]
                    {
                        let proxy = setting_proxy.clone();
                        if let Err(error) = setting_window::open(Box::new(move || {
                            let _ = proxy.send_event(Wake::SettingSaved);
                        })) {
                            diagnostic::log("setting", format!("{error:#}"));
                        }
                    }
                }

                #[cfg(not(target_os = "linux"))]
                control::Command::Toggle => {
                    let _ = gesture_proxy.send_event(Wake::Gesture);
                }

                // Ends the take and throws the words away. The distinction from
                // Toggle is the whole reason it exists: stopping inserts what
                // you said, and the case this is for is realising you were
                // dictating into the wrong window.
                control::Command::Cancel => {
                    if let Some(running) = take.take() {
                        diagnostic::log("control", "cancelled — the take is discarded");
                        running.discard();
                        #[cfg(not(target_os = "linux"))]
                        wear(status.as_mut(), floating.as_ref(), tray::Mood::Idle);
                    }
                }

                #[cfg(target_os = "linux")]
                control::Command::Toggle => {
                    diagnostic::log("control", "no tray on Linux — nothing to toggle from here");
                }
            },

            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::Silence(quiet)) => {
                quiet_since = quiet.then(Instant::now);
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

            // Same refusal as the menu item, for the same reason: a rebind
            // that lands while the user is holding the old binding moves the
            // key underneath them. The file already has the change, so the next
            // reload — a click, or the next save — applies it.
            #[cfg(not(target_os = "linux"))]
            Event::UserEvent(Wake::SettingSaved) => {
                if take.is_some() {
                    diagnostic::log(
                        "setting",
                        "saved from the settings window, but a take is on the air —                          it will apply at the next reload",
                    );
                } else {
                    file_broken = !reload(
                        &mut setting,
                        hotkey.as_mut(),
                        &injector,
                        &mut stop_on_return,
                        status.as_mut(),
                    );
                    reconcile_floating(&setting, &mut floating, &floating_proxy);
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

                // A click is a tap, and says so in edges rather than in state.
                // Starting a take from here by hand would mean a second copy of
                // the start path — the report, the grip, the Return watcher, the
                // icon, the credential re-read on failure — and two copies of
                // "when may a take begin" is how the overlap guard gets a hole
                // in it. Posting the gesture instead means the menu can only ask
                // for what a person with a keyboard could already have asked for.
                tray::Ask::ToggleDictation => {
                    let _ = gesture_proxy.send_event(Wake::Gesture);
                }

                // The one warning in the menu whose fix is a command. Putting
                // it on the clipboard is `showCredentialHelp` in the Swift
                // build, and for its reason: the fix has to be typed into a
                // terminal, and nobody should have to transcribe it from a menu
                // that closed when they clicked it.
                tray::Ask::CredentialHelp => {
                    let copied = arboard::Clipboard::new()
                        .and_then(|mut board| board.set_text(CREDENTIAL_FIX.to_string()));
                    match copied {
                        Ok(()) => diagnostic::log(
                            "credential",
                            format!("`{CREDENTIAL_FIX}` is on the clipboard — run it in a terminal, then click Reload Settings"),
                        ),
                        Err(error) => diagnostic::log(
                            "credential",
                            format!("could not copy the fix to the clipboard: {error} — run `{CREDENTIAL_FIX}` in a terminal"),
                        ),
                    }
                }

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

                // The window writes the file and nothing else — see its own
                // header. What it asks for when it has written is exactly this
                // arm, so a change made in the window and a change made in a
                // text editor reach the running app by the same path.
                tray::Ask::OpenSetting => {
                    let proxy = setting_proxy.clone();
                    if let Err(error) = setting_window::open(Box::new(move || {
                        let _ = proxy.send_event(Wake::SettingSaved);
                    })) {
                        diagnostic::log("setting", format!("{error:#}"));
                    }
                }

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
                        reconcile_floating(&setting, &mut floating, &floating_proxy);
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

                tray::Ask::Ignore => {}
            },

            Event::LoopDestroyed => {
                if let Some(running) = take.take() {
                    running.finish();
                }
                // Before the tray icon goes: a topmost window left behind by a
                // process that has exited is the one piece of this app a user
                // cannot get rid of without the task manager.
                #[cfg(not(target_os = "linux"))]
                if let Some(button) = floating.take() {
                    button.close();
                }
                drop(hotkey.take());
                // Dropping the last handle is what removes the icon; leaving it
                // to process teardown leaves a ghost in the tray until hover.
                #[cfg(not(target_os = "linux"))]
                drop(status.take());
            }
            _ => {}
        }

        // The latch timeout, decided after every event rather than inside any
        // one of them, because it depends on three things that change in
        // different arms: whether a take is latched, whether the microphone is
        // quiet, and how long it has been.
        //
        // A held take is never ended here. The key is the contract — it ends
        // when the user lets go — and a hold that outlived the timeout is
        // someone still pressing the button, not someone who forgot.
        #[cfg(not(target_os = "linux"))]
        if *flow != ControlFlow::Exit {
            match latch(
                take.as_ref().map(|running| running.grip),
                quiet_since,
                setting.latch_timeout_second,
                Instant::now(),
            ) {
                Latch::Expired => {
                    if let Some(running) = take.take() {
                        diagnostic::log(
                            "take",
                            format!(
                                "latched and silent for {}s — ending the take",
                                setting.latch_timeout_second
                            ),
                        );
                        running.finish();
                    }
                    quiet_since = None;
                    wear(status.as_mut(), floating.as_ref(), tray::Mood::Idle);
                }
                // Wake exactly when it expires and not before. Without this the
                // loop would sleep until the next keystroke or audio event, and
                // a microphone left open in a silent room produces neither.
                Latch::WaitUntil(at) => *flow = ControlFlow::WaitUntil(at),
                Latch::Nothing => {}
            }
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
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    let level = level.clone();
    let quiet_proxy = level.clone();
    let transcript = transcript.clone();
    // Atomic rather than a `Cell` because the audio callback hands out `Fn`,
    // not `FnMut`, and it runs on a thread neither the loop nor this function
    // owns. `u8::MAX` is not a step, so the first buffer always reports.
    let last = AtomicU8::new(u8::MAX);
    // A take starts with somebody about to speak, so it starts loud. Beginning
    // at "quiet" would arm the latch timeout before a word was said.
    let was_quiet = AtomicBool::new(false);

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

            // Silence is asked of the raw reading rather than of the step
            // above, and the difference matters. A step is a fifth of full
            // scale, so step zero covers everything under 20% — which includes
            // a quiet speaker talking normally. Cutting their take off would be
            // a far worse bug than the one the latch timeout fixes. This
            // threshold is the noise floor of a room, not the bottom of the
            // meter.
            let quiet = reading < SILENCE_LEVEL;
            if was_quiet.swap(quiet, Ordering::Relaxed) != quiet {
                let _ = quiet_proxy.send_event(Wake::Silence(quiet));
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
    // Not part of `Capability`, which answers what this build can do, while
    // this answers what this machine currently has plugged in. It belongs in
    // the report either way: no input device is the most common reason a take
    // does nothing, and it is invisible until someone presses the key.
    match splaude_platform::audio::default_input_name() {
        Some(name) => println!("  microphone     {name}"),
        None => println!("  microphone     none — dictation cannot start without one"),
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
    // Which provider a take would actually reach, and whether it could. A user
    // who has switched providers and is getting nothing needs to see both
    // halves here rather than discovering the empty key when they press the
    // key.
    match setting.speech_backend.trim() {
        "deepgram" if setting.api_key.trim().is_empty() => {
            println!("  backend   deepgram — but apiKey is empty, so no take can start")
        }
        "deepgram" => println!("  backend   deepgram (your own key)"),
        "anthropic" | "" => println!("  backend   anthropic (the Claude Code credential)"),
        other => println!("  backend   {other} is not a known backend — anthropic will be used"),
    }
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

    Ok(())
}

/// Brings the floating button into line with the settings, after a reload.
///
/// Only its presence, not its position: the button is the authority on where it
/// is — the user drags it — and a reload that moved it back to whatever the file
/// last recorded would fight them.
#[cfg(not(target_os = "linux"))]
fn reconcile_floating(
    setting: &Setting,
    floating: &mut Option<floating::Handle>,
    proxy: &EventLoopProxy<Wake>,
) {
    match (setting.show_floating_button, floating.is_some()) {
        (true, false) => *floating = raise_floating(setting, proxy),
        (false, true) => {
            if let Some(button) = floating.take() {
                button.close();
            }
        }
        _ => {}
    }
}

/// Puts the same mood on every surface that shows one.
///
/// The tray and the floating button are two views of one fact — whether a take
/// is on the air, and how loud it is — and the way that goes wrong is one of
/// them being updated at a call site that forgot the other. There is one call
/// site shape instead, and both are absent-safe.
#[cfg(not(target_os = "linux"))]
fn wear(status: Option<&mut tray::Tray>, floating: Option<&floating::Handle>, mood: tray::Mood) {
    if let Some(shown) = status {
        shown.set_mood(mood);
    }
    if let Some(button) = floating {
        button.set_mood(mood);
    }
}

/// Shows the floating button, if the settings ask for one.
///
/// A failure here is logged and swallowed. The button is a convenience over a
/// hotkey that still works, and refusing to start the app because a decoration
/// could not be drawn would trade a small loss for the whole thing.
#[cfg(not(target_os = "linux"))]
fn raise_floating(setting: &Setting, proxy: &EventLoopProxy<Wake>) -> Option<floating::Handle> {
    if !setting.show_floating_button {
        return None;
    }

    let clicked = proxy.clone();
    let moved = proxy.clone();

    match floating::show(
        setting.floating_button_point,
        Box::new(move || {
            let _ = clicked.send_event(Wake::Gesture);
        }),
        Box::new(move |point| {
            let _ = moved.send_event(Wake::FloatingMoved(point));
        }),
    ) {
        Ok(button) => Some(button),
        Err(error) => {
            diagnostic::log("floating", format!("{error:#}"));
            None
        }
    }
}

/// Opens the settings window and waits for it to close.
///
/// Nothing to tell about a save here: there is no event loop running to hear
/// it, and the window has already written the file — the next `splaude` to
/// start reads it.
#[cfg(not(target_os = "linux"))]
fn settings() -> Result<()> {
    // The window writes the settings file and can fail doing it — a read-only
    // directory, a disk that is full — and on this path there is no tray, no
    // console anybody is watching, and until now no log either. That made
    // `--settings` the one entry point where a save could fail in total
    // silence, which is the failure mode every other part of this app has
    // already been taught not to have.
    diagnostic::to_file();

    match setting_window::open(Box::new(|| {}))? {
        Some(window) => {
            // The error is the thread having panicked, which is not something
            // this process can do anything about beyond not pretending it went
            // well.
            window
                .join()
                .map_err(|_| anyhow::anyhow!("the settings window ended unexpectedly"))?;
            Ok(())
        }
        // Only reachable if a window were somehow already open in this same
        // process, which cannot happen on this path.
        None => Ok(()),
    }
}

/// Tells Windows this process does its own scaling.
///
/// Without it a process is *DPI-unaware*, and Windows compensates by rendering
/// its windows at 96 DPI and bitmap-stretching the result — so on a 150%
/// display the settings window and the floating button come up the right size
/// and visibly blurred, with text that looks like a photograph of text. That is
/// the single loudest way a hand-written Win32 window announces that nobody
/// looked at it.
///
/// System-aware rather than per-monitor: the layout is computed once when a
/// window is created, and per-monitor awareness promises something this code
/// does not do — relaying out when a window is dragged between two displays at
/// different scales. Claiming it and then not honouring it would be worse than
/// not claiming it, because Windows would stop compensating for the case it
/// currently handles.
///
/// Called before anything creates a window, which is what the API requires; it
/// is ignored once one exists. Failure is not worth reporting — the answer is
/// the old blurry behaviour, not a broken app.
#[cfg(windows)]
fn declare_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    };

    // SAFETY: takes a context constant by value and returns whether it was
    // accepted. No pointers, and nothing borrowed.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
    }
}

/// Nothing to declare. macOS and Linux scale by points and backing factors
/// rather than by asking the process to opt in.
#[cfg(not(windows))]
fn declare_dpi_awareness() {}

/// Hands a command to the running instance and says what happened.
///
/// An exit code of 1 when nobody is listening, because this is a command
/// somebody has bound to a key or put in a script: "there was nothing to talk
/// to" has to be distinguishable from "done" without reading the words.
fn forward(command: control::Command) -> Result<()> {
    match control::send(command)? {
        control::Delivery::Taken => Ok(()),
        control::Delivery::NobodyListening => {
            bail!("splaude is not running — start it first")
        }
    }
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

#[cfg(test)]
mod test {

    /// The latch timeout, which cannot be exercised in a real take on a machine
    /// with no microphone — so it is a pure rule with a clock passed in.
    #[cfg(not(target_os = "linux"))]
    mod latching {
        use super::*;

        fn now() -> Instant {
            Instant::now()
        }

        #[test]
        fn a_latched_take_that_has_been_quiet_long_enough_expires() {
            let quiet = now();
            let later = quiet + Duration::from_secs(31);
            assert_eq!(
                latch(Some(Grip::Latched), Some(quiet), 30, later),
                Latch::Expired
            );
        }

        #[test]
        fn a_latched_take_that_has_not_waited_long_enough_sleeps_until_it_has() {
            let quiet = now();
            let soon = quiet + Duration::from_secs(5);
            assert_eq!(
                latch(Some(Grip::Latched), Some(quiet), 30, soon),
                Latch::WaitUntil(quiet + Duration::from_secs(30))
            );
        }

        /// The rule that keeps this from being a bug: a key that is still down
        /// is somebody still talking into it, however long the silence.
        #[test]
        fn a_held_take_is_never_ended_by_silence() {
            let quiet = now();
            let much_later = quiet + Duration::from_secs(600);
            assert_eq!(
                latch(Some(Grip::Held(quiet)), Some(quiet), 30, much_later),
                Latch::Nothing
            );
        }

        #[test]
        fn a_take_that_is_not_quiet_is_not_waiting_on_anything() {
            assert_eq!(latch(Some(Grip::Latched), None, 30, now()), Latch::Nothing);
        }

        #[test]
        fn no_take_means_nothing_to_end() {
            assert_eq!(latch(None, Some(now()), 30, now()), Latch::Nothing);
        }

        /// Zero is the documented way to turn it off, and it must hold even for
        /// a take that has been silent for an hour.
        #[test]
        fn a_timeout_of_zero_never_ends_a_take() {
            let quiet = now();
            let much_later = quiet + Duration::from_secs(3_600);
            assert_eq!(
                latch(Some(Grip::Latched), Some(quiet), 0, much_later),
                Latch::Nothing
            );
        }

        /// Exactly at the threshold counts as expired, so a timeout of N is
        /// honoured at N rather than at N plus one event.
        #[test]
        fn the_boundary_expires_rather_than_waiting_one_more_round() {
            let quiet = now();
            let exactly = quiet + Duration::from_secs(30);
            assert_eq!(
                latch(Some(Grip::Latched), Some(quiet), 30, exactly),
                Latch::Expired
            );
        }

        /// A clock read on the audio thread can land marginally ahead of this
        /// one; subtracting it must not panic.
        #[test]
        fn an_instant_from_the_future_does_not_panic() {
            let quiet = now() + Duration::from_secs(5);
            assert!(matches!(
                latch(Some(Grip::Latched), Some(quiet), 30, now()),
                Latch::WaitUntil(_)
            ));
        }
    }
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
