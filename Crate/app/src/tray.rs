//! The status-bar icon, on the platforms that have one.
//!
//! Windows and macOS only — see the `cfg` on the dependency in `Cargo.toml`.
//! It does two things. It is the only visible evidence the process is alive:
//! there is no window, and after startup there is no console output either, so
//! without it a running splaude is indistinguishable from one that failed to
//! start. And it carries Quit, which is the only orderly way out — `Ctrl+C`
//! kills the process without ever reaching `LoopDestroyed`, so the hotkey stays
//! registered with the OS and that chord is dead for every other app until the
//! session ends.
//!
//! The icon is drawn rather than shipped as a file: an image asset means an
//! image decoder in the dependency tree, and `Resource/splaude.icns` is
//! macOS-only anyway. The drawing itself lives in [`crate::icon`], which is
//! also what `build.rs` renders the embedded `.ico` from — one renderer, so the
//! tray and the Explorer icon cannot drift apart the way the `.icns` did.

use anyhow::{anyhow, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use splaude_core::{diagnostic, quota, update, Setting};

use crate::icon;

/// Re-exported so `main.rs` can name it without depending on `tray-icon`
/// directly — the whole point being that on Linux neither exists.
pub use tray_icon::menu::MenuId;

/// Re-exported for the same reason the mark moved out at all: the event loop
/// thinks in terms of the tray, and where the pixels come from is this module's
/// business rather than its caller's.
pub use crate::icon::{step_of, Mood};

// Namespaced because `muda` ids share one global event stream, and constant
// across menu rebuilds so a click always means the same thing.
const QUIT: &str = "splaude:quit";
const TRANSCRIPT: &str = "splaude:transcript";
const REVEAL_LOG: &str = "splaude:reveal-log";
const AUTOSTART: &str = "splaude:autostart";
const TEST_PASTE: &str = "splaude:test-paste";
const EDIT_SETTING: &str = "splaude:edit-setting";
const RELOAD_SETTING: &str = "splaude:reload-setting";
const UPDATE: &str = "splaude:update";

/// Longest transcript preview the menu will show, in characters.
///
/// A menu is as wide as its widest item, so a sentence of dictation dropped in
/// untruncated stretches the whole thing across the screen. The Swift build
/// dodged this with a fixed "Copy Last Transcript" title and the text in a
/// tooltip; showing a clipped preview is the same trade with the words visible.
const PREVIEW_LIMIT: usize = 40;

/// What a menu click is asking for.
///
/// The tray does not act on these itself: Quit has to reach the event loop,
/// which owns the only orderly shutdown path, and launch-at-login has to reach
/// the [`splaude_core::Setting`] the loop holds. Returning intent keeps both
/// decisions where the state is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask {
    Quit,
    CopyTranscript,
    RevealLog,
    ToggleAutostart,
    /// Type a known string into whatever has focus, exercising injection alone.
    TestPaste,
    /// Open the settings file in whatever edits JSON on this machine.
    EditSetting,
    /// Re-read the settings file and apply what can be applied live.
    ReloadSetting,
    /// The update item. What it means depends on what the last check found —
    /// open the release page, or go and look again — and the caller holds that
    /// reading, so the click stays one `Ask` rather than two the tray would
    /// have to choose between.
    Update,
    /// A click on a label, or on an id from some other `muda` consumer.
    Ignore,
}

pub struct Tray {
    icon: TrayIcon,
    mood: Mood,
    hotkey: String,
    /// The credential sentence, or `None` when there is nothing worth saying.
    health: Option<String>,
    /// What is wrong with the settings file, or `None` when it read cleanly.
    note: Option<String>,
    /// The last take's committed text in full. The menu shows a clipped
    /// preview; the clipboard gets all of it.
    transcript: Option<String>,
    autostart: bool,
    /// What the last update check found. Held rather than read live, unlike the
    /// quota line, because this one costs a network request — a menu that
    /// checked GitHub every time it was drawn would spend its hourly allowance
    /// on someone opening the menu.
    update: update::Reading,
}

impl Tray {
    /// Create the icon. Call this once the loop is already running rather than
    /// before it: `tray-icon` documents that a macOS status item built before
    /// the run loop starts misbehaves around fullscreen apps.
    pub fn new(hotkey: &str, health: Option<String>, autostart: bool) -> Result<Self> {
        let icon = TrayIconBuilder::new()
            .with_tooltip(tooltip(Mood::Idle, hotkey))
            .with_icon(image(Mood::Idle)?)
            .build()
            .map_err(|error| anyhow!("could not create the tray icon: {error}"))?;

        // Nothing here wants click events, but an unread `TrayIconEvent` is not
        // discarded — it is queued on an unbounded channel forever, and `Move`
        // fires for every pixel the pointer crosses. Swallow them at the source.
        TrayIconEvent::set_event_handler(Some(|_| {}));

        announce();

        let tray = Self {
            icon,
            mood: Mood::Idle,
            hotkey: hotkey.to_string(),
            health,
            note: None,
            transcript: None,
            autostart,
            update: update::Reading::Unknown,
        };
        tray.rebuild()?;

        Ok(tray)
    }

    /// Reflect the take lifecycle and the input level. Cheap to call on every
    /// edge — a repeat is dropped before it reaches the platform.
    pub fn set_mood(&mut self, mood: Mood) {
        if self.mood == mood {
            return;
        }
        self.mood = mood;

        // Both failures are cosmetic. A stale icon is not worth interrupting a
        // take that is otherwise working.
        if let Ok(icon) = image(mood) {
            let _ = self.icon.set_icon(Some(icon));
        }
        let _ = self.icon.set_tooltip(Some(tooltip(mood, &self.hotkey)));
    }

    /// Whether a take is currently on the air, so a level arriving late — the
    /// audio thread is still draining when the key comes up — cannot re-redden
    /// an icon that has already gone idle.
    pub fn is_recording(&self) -> bool {
        matches!(self.mood, Mood::Recording(_))
    }

    /// Show, update, or drop the credential warning.
    pub fn set_health(&mut self, health: Option<String>) {
        if self.health == health {
            return;
        }
        if let Some(line) = &health {
            diagnostic::log("credential", line);
        }
        self.health = health;
        self.redraw_menu();
    }

    /// Remember the last take's words. Empty text is not a take worth
    /// offering — it is a take that produced nothing.
    pub fn set_transcript(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.transcript = Some(text.to_string());
        self.redraw_menu();
    }

    /// Show, update, or drop the settings-file complaint.
    ///
    /// The point of the item is that a hand-edited file which failed to parse
    /// stops being a silent revert to defaults. A user who mistyped a comma has
    /// lost their hotkey and their keyterms, and this is the only place they
    /// would find out without opening the log.
    pub fn set_note(&mut self, note: Option<String>) {
        if self.note == note {
            return;
        }
        self.note = note;
        self.redraw_menu();
    }

    /// Rename the push-to-talk key in the label and the tooltip, after a
    /// reload moved it.
    pub fn set_hotkey(&mut self, hotkey: &str) {
        if self.hotkey == hotkey {
            return;
        }
        self.hotkey = hotkey.to_string();
        let _ = self
            .icon
            .set_tooltip(Some(tooltip(self.mood, &self.hotkey)));
        self.redraw_menu();
    }

    /// Rebuild so the quota line is re-read. Nothing else here changes.
    pub fn refresh(&self) {
        self.redraw_menu();
    }

    pub fn set_autostart(&mut self, enabled: bool) {
        if self.autostart == enabled {
            return;
        }
        self.autostart = enabled;
        self.redraw_menu();
    }

    /// Put the last take's words on the clipboard, in full.
    ///
    /// The handle is built per click and dropped again: on every platform an
    /// open clipboard is a shared resource other apps are queueing for, and
    /// holding one for the life of a background process is how you become the
    /// reason someone else's copy fails.
    pub fn copy_transcript(&self) {
        let Some(text) = &self.transcript else {
            return;
        };

        let copied = arboard::Clipboard::new().and_then(|mut board| board.set_text(text.clone()));
        match copied {
            Ok(()) => diagnostic::log(
                "tray",
                format!("copied {} chars to the clipboard", text.chars().count()),
            ),
            Err(error) => diagnostic::log("tray", format!("could not copy: {error}")),
        }
    }

    /// What the last update check found.
    pub fn set_update(&mut self, reading: update::Reading) {
        if self.update == reading {
            return;
        }
        self.update = reading;
        self.redraw_menu();
    }

    /// `muda` has no notion of a hidden item, so an item that is sometimes
    /// absent means replacing the menu rather than editing it. That is also
    /// how the Swift build works — `buildMenu()` makes a fresh `NSMenu` every
    /// time — and at these rates (a credential poll every five minutes, one
    /// transcript per take, a click) rebuilding costs nothing worth saving.
    fn rebuild(&self) -> Result<()> {
        let menu = Menu::new();

        // Above everything, so a dead credential is the first thing read
        // rather than something discovered when a take fails. Disabled: it is
        // a warning, and an enabled item that does nothing when clicked reads
        // as broken.
        if let Some(line) = &self.health {
            append(&menu, &MenuItem::new(line, false, None))?;
        }
        // Beside the credential warning rather than beside the settings items:
        // both are the same kind of statement — something the app needs is not
        // in the state you think it is — and both are read before anything is
        // clicked.
        if let Some(line) = &self.note {
            append(&menu, &MenuItem::new(line, false, None))?;
        }
        // An available update is promoted into the same block, and only when
        // there is one. It is the same kind of statement as the two above —
        // something is not in the state you assume — and the bottom of a menu
        // is where a notice goes to be unread. Every other reading stays in the
        // diagnostics group below, because "you are up to date" is an answer to
        // a question, not news.
        let promoted = self.update.is_worth_saying();
        if promoted {
            append(
                &menu,
                &MenuItem::with_id(UPDATE, update_line(&self.update), true, None),
            )?;
        }
        if self.health.is_some() || self.note.is_some() || promoted {
            append(&menu, &PredefinedMenuItem::separator())?;
        }

        if let Some(text) = &self.transcript {
            append(
                &menu,
                &MenuItem::with_id(TRANSCRIPT, clip(text, PREVIEW_LIMIT), true, None),
            )?;
        }

        // Disabled on purpose — it is a label.
        append(
            &menu,
            &MenuItem::new(
                format!("Hold {} to talk, or tap to latch", self.hotkey),
                false,
                None,
            ),
        )?;
        // Also a label. Read live rather than stored on `Tray`, so it is
        // current as of whatever rebuilt the menu — a take, a reload, a health
        // poll — rather than as of whenever the last quota event happened to be
        // pushed at us.
        append(
            &menu,
            &MenuItem::new(quota_line(&quota::reading()), false, None),
        )?;
        append(&menu, &PredefinedMenuItem::separator())?;

        append(
            &menu,
            &CheckMenuItem::with_id(AUTOSTART, "Launch at login", true, self.autostart, None),
        )?;
        // The settings pair together, above the diagnostics: the file is the
        // interface, so opening it and re-reading it are one gesture in two
        // halves.
        append(
            &menu,
            &MenuItem::with_id(EDIT_SETTING, "Edit Settings…", true, None),
        )?;
        append(
            &menu,
            &MenuItem::with_id(RELOAD_SETTING, "Reload Settings", true, None),
        )?;
        // The two diagnostics together, above the log they write into. A user
        // who dictated and saw nothing reaches for these in order: prove the
        // keystrokes land, then read what the take actually did.
        append(
            &menu,
            &MenuItem::with_id(TEST_PASTE, "Test Paste", true, None),
        )?;
        append(
            &menu,
            &MenuItem::with_id(REVEAL_LOG, "Reveal Log", true, None),
        )?;
        // Unless it was promoted above, where a duplicate id would give the
        // same click two places to come from.
        if !promoted {
            append(
                &menu,
                &MenuItem::with_id(UPDATE, update_line(&self.update), true, None),
            )?;
        }
        append(&menu, &PredefinedMenuItem::separator())?;
        append(&menu, &MenuItem::with_id(QUIT, "Quit splaude", true, None))?;

        self.icon.set_menu(Some(Box::new(menu)));
        Ok(())
    }

    /// Menu content is cosmetic; a take in flight is not. A rebuild that fails
    /// leaves the previous menu in place, which still carries Quit.
    fn redraw_menu(&self) {
        if let Err(error) = self.rebuild() {
            diagnostic::log("tray", format!("{error:#}"));
        }
    }
}

/// What a menu event is asking for.
pub fn ask(id: &MenuId) -> Ask {
    match id.as_ref() {
        QUIT => Ask::Quit,
        TRANSCRIPT => Ask::CopyTranscript,
        REVEAL_LOG => Ask::RevealLog,
        AUTOSTART => Ask::ToggleAutostart,
        TEST_PASTE => Ask::TestPaste,
        EDIT_SETTING => Ask::EditSetting,
        RELOAD_SETTING => Ask::ReloadSetting,
        UPDATE => Ask::Update,
        _ => Ask::Ignore,
    }
}

/// How the handshake's rate-limit evidence reads in a menu.
///
/// The README's central claim is that dictation does not spend Claude quota,
/// and these headers are the whole of the client-side evidence for it: an
/// endpoint that metered the request answers with `anthropic-ratelimit-*`. Up
/// to now that evidence only ever reached the log. Pure over the reading so the
/// wording can be checked without a desktop, a socket or a take.
fn quota_line(reading: &quota::Reading) -> String {
    format!("Claude quota: {}", reading.line())
}

/// How an update reading reads as a clickable item.
///
/// Every one of these is enabled, and each one does something when clicked:
/// the available reading opens the release page, and the other three go and
/// look again. That is why none of them is phrased as a bare statement — an
/// item that reads "0.2.0 is the latest" and does nothing would be a label, and
/// this is not one. The trailing ellipsis marks the two that leave the app.
fn update_line(reading: &update::Reading) -> String {
    match reading {
        update::Reading::Unknown => "Check for Updates".into(),
        update::Reading::Current => format!("splaude {} is the latest", update::current()),
        update::Reading::Available(release) => format!("Update to {}…", release.version),
        update::Reading::Failed(_) => "Check for Updates (last try failed)".into(),
    }
}

/// Open the settings file in whatever this machine edits JSON with.
///
/// Written out first if it is not there. An editor opened on a file that does
/// not exist shows an empty buffer, which tells the user nothing about what is
/// configurable and invites them to invent a schema; the current state written
/// out is both a working file and the documentation for it.
///
/// Same shape as [`reveal_log`] — a spawned platform opener, no dependency —
/// except that this opens the file rather than revealing it, because unlike the
/// log a `.json` is something the machine plausibly has a handler for.
pub fn edit_setting(setting: &Setting) {
    let path = Setting::path();

    if !path.exists() {
        match setting.save() {
            Ok(()) => diagnostic::log("setting", format!("wrote {}", path.display())),
            Err(error) => {
                diagnostic::log(
                    "setting",
                    format!("could not write {}: {error}", path.display()),
                );
                return;
            }
        }
    }

    hand_to_desktop("setting", path.as_os_str());
}

/// Hand something to whatever this desktop opens it with.
///
/// A path or a URL — the platform openers take either, which is the whole
/// reason this is one function and not two. Extracted when the release page
/// became the third caller; three copies of a `cfg` ladder is where one of them
/// starts quietly differing from the others.
///
/// `topic` names the caller in the log, so a failure says which gesture failed
/// rather than only that an opener did.
fn hand_to_desktop(topic: &str, target: &std::ffi::OsStr) {
    // `start` is a shell builtin, not an executable, so it needs `cmd`. The
    // empty first argument is the window title `start` would otherwise take the
    // target for — without it a quoted path becomes the title and nothing opens.
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd")
        .args(["/c", "start", ""])
        .arg(target)
        .spawn();

    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(target).spawn();

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(target).spawn();

    if let Err(error) = spawned {
        diagnostic::log(
            topic,
            format!("could not open {}: {error}", target.to_string_lossy()),
        );
    }
}

/// Open a release page in the browser.
///
/// Only ever called with a URL that came out of [`splaude_core::update`], which
/// takes it from GitHub's own answer for the one repository this build names.
/// Worth stating because handing an arbitrary string to the platform opener is
/// how a link becomes a command.
pub fn open_release(url: &str) {
    diagnostic::log("update", format!("opening {url}"));
    hand_to_desktop("update", std::ffi::OsStr::new(url));
}

/// Show the log file in the platform's file manager, selected.
///
/// Opening the *folder* rather than the file: the log has no registered
/// handler on Windows, so opening it directly lands on "how do you want to
/// open this?", and revealing it is what the Swift build does with
/// `NSWorkspace.activateFileViewerSelecting` anyway.
pub fn reveal_log() {
    let path = diagnostic::path();

    // `explorer.exe` exits non-zero even when it did the right thing, so its
    // status is not worth reading — only failing to spawn at all is news.
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();

    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .arg("-R")
        .arg(&path)
        .spawn();

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let spawned: std::io::Result<std::process::Child> = Err(std::io::Error::other(
        "no file manager is known on this platform",
    ));

    if let Err(error) = spawned {
        diagnostic::log(
            "tray",
            format!("could not reveal {}: {error}", path.display()),
        );
    }
}

/// Shorten `text` to `limit` characters for a menu item, on characters rather
/// than bytes so a multibyte transcript is not cut mid-glyph. Newlines become
/// spaces: a menu item is one line whatever the string says.
fn clip(text: &str, limit: usize) -> String {
    let single_line: String = text
        .chars()
        .map(|letter| if letter.is_control() { ' ' } else { letter })
        .collect();

    if single_line.chars().count() <= limit {
        return single_line;
    }
    single_line
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Hand menu clicks to the event loop.
///
/// The handler runs on whatever thread the platform dispatches the menu on, so
/// it does nothing but wake the loop — same shape as the hotkey callback.
pub fn forward<T: Send + 'static>(
    proxy: tao::event_loop::EventLoopProxy<T>,
    wrap: fn(MenuId) -> T,
) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(wrap(event.id));
    }));
}

/// Windows 11 files every new tray icon into the hidden-icons overflow. A user
/// who does not know that looks at the taskbar, sees nothing, and concludes the
/// app did not start — which has already happened.
#[cfg(target_os = "windows")]
fn announce() {
    println!(
        "The tray icon starts hidden: open the hidden-icons flyout (the ^ on the taskbar) and \
         drag splaude onto the taskbar to keep it visible."
    );
}

#[cfg(not(target_os = "windows"))]
fn announce() {}

fn append(menu: &Menu, item: &dyn tray_icon::menu::IsMenuItem) -> Result<()> {
    menu.append(item)
        .map_err(|error| anyhow!("could not build the tray menu: {error}"))
}

fn tooltip(mood: Mood, hotkey: &str) -> String {
    match mood {
        Mood::Idle => format!("splaude — hold {hotkey} to dictate"),
        Mood::Recording(_) => "splaude — recording".to_string(),
    }
}

fn image(mood: Mood) -> Result<Icon> {
    Icon::from_rgba(icon::rgba(mood), icon::EDGE, icon::EDGE)
        .map_err(|error| anyhow!("could not build the tray image: {error}"))
}

#[cfg(test)]
mod test {
    use super::*;

    // No test here builds a real tray icon, opens a clipboard or spawns a file
    // manager: CI has no desktop session.

    #[test]
    fn a_short_transcript_is_shown_whole() {
        assert_eq!(clip("hello there", 40), "hello there");
        // Exactly at the limit is still whole — the ellipsis would cost a
        // character to save none.
        assert_eq!(clip("0123456789", 10), "0123456789");
    }

    #[test]
    fn a_long_transcript_is_clipped_to_the_limit() {
        let clipped = clip("0123456789abcdef", 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('…'));
        assert!(clipped.starts_with("012345678"));
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        // A menu is measured in glyphs, and cutting a multibyte transcript on
        // a byte boundary would panic rather than merely look wrong.
        let wide = "日本語のテキストがここにあります";
        assert_eq!(clip(wide, 5).chars().count(), 5);
        assert_eq!(clip("héllo", 10), "héllo");
    }

    #[test]
    fn a_newline_does_not_break_the_menu_item() {
        // A menu item is one line whatever the recogniser handed back.
        assert_eq!(clip("one\ntwo\rthree", 40), "one two three");
    }
    #[test]
    fn every_actionable_id_maps_to_its_own_ask() {
        assert_eq!(ask(&MenuId::new(QUIT)), Ask::Quit);
        assert_eq!(ask(&MenuId::new(TRANSCRIPT)), Ask::CopyTranscript);
        assert_eq!(ask(&MenuId::new(REVEAL_LOG)), Ask::RevealLog);
        assert_eq!(ask(&MenuId::new(AUTOSTART)), Ask::ToggleAutostart);
        assert_eq!(ask(&MenuId::new(TEST_PASTE)), Ask::TestPaste);
        assert_eq!(ask(&MenuId::new(EDIT_SETTING)), Ask::EditSetting);
        assert_eq!(ask(&MenuId::new(RELOAD_SETTING)), Ask::ReloadSetting);
        assert_eq!(ask(&MenuId::new(UPDATE)), Ask::Update);
    }

    fn published(major: u32, minor: u32, patch: u32) -> update::Release {
        update::Release {
            version: update::Version {
                major,
                minor,
                patch,
            },
            url: "https://example.invalid/release".into(),
        }
    }

    /// Only the available reading is promoted into the top block, so only it may
    /// claim the position the credential warning uses. The other three are
    /// answers to a question, and a menu that opens with "you are up to date"
    /// has spent its most-read line on nothing.
    #[test]
    fn only_an_available_update_is_promoted() {
        assert!(update::Reading::Available(published(9, 9, 9)).is_worth_saying());
        for quiet in [
            update::Reading::Unknown,
            update::Reading::Current,
            update::Reading::Failed("offline".into()),
        ] {
            assert!(!quiet.is_worth_saying(), "{quiet:?} would be promoted");
        }
    }

    /// Every one of these is a clickable item, so none may read as a dead label
    /// and none may render empty — including the failed one, which is the whole
    /// way back from a check that did not answer.
    #[test]
    fn every_update_line_fits_a_menu_item() {
        for reading in [
            update::Reading::Unknown,
            update::Reading::Current,
            update::Reading::Available(published(1, 2, 3)),
            update::Reading::Failed("timed out".into()),
        ] {
            let line = update_line(&reading);
            assert!(!line.trim().is_empty(), "{reading:?} rendered empty");
            assert!(line.chars().count() <= 60, "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
    }

    /// The version has to reach the item, or it says an update exists without
    /// saying which — and the click that follows is a leap of faith.
    #[test]
    fn the_available_line_names_the_version() {
        let line = update_line(&update::Reading::Available(published(1, 2, 3)));
        assert!(line.contains("1.2.3"), "{line}");
    }

    /// A failed check must not read like a successful one. The distinction is
    /// the same one the quota line makes: not knowing is not the same as knowing
    /// there is nothing.
    #[test]
    fn a_failed_check_does_not_read_as_up_to_date() {
        let failed = update_line(&update::Reading::Failed("offline".into()));
        let current = update_line(&update::Reading::Current);
        assert_ne!(failed, current);
        assert!(failed.contains("failed"), "{failed}");
    }

    #[test]
    fn the_quota_line_keeps_the_never_dictated_state_distinct() {
        // Flattening these two would turn "we have not asked yet" into a claim
        // that nothing was metered, which is the one thing this line must not
        // say without evidence.
        let unknown = quota_line(&quota::Reading::Unknown);
        let unmetered = quota_line(&quota::Reading::Unmetered);
        assert_ne!(unknown, unmetered);
        assert!(unknown.contains("dictate once"), "{unknown}");
        assert!(unmetered.contains("nothing metered"), "{unmetered}");
    }

    #[test]
    fn the_quota_line_shows_a_reading_it_was_given() {
        let line = quota_line(&quota::Reading::Metered(
            "anthropic-ratelimit-requests-remaining=42".into(),
        ));
        assert!(line.contains("42"), "{line}");
    }

    #[test]
    fn every_quota_line_fits_a_menu_item() {
        // A menu is as wide as its widest item, and this one is always present.
        for reading in [
            quota::Reading::Unknown,
            quota::Reading::Unmetered,
            quota::Reading::Unavailable,
        ] {
            let line = quota_line(&reading);
            assert!(!line.is_empty());
            assert!(line.chars().count() <= 60, "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
    }

    #[test]
    fn every_id_is_distinct() {
        // Two items sharing an id would silently make one of them do the
        // other's job, and `muda` would not complain.
        let id = [
            QUIT,
            TRANSCRIPT,
            REVEAL_LOG,
            AUTOSTART,
            TEST_PASTE,
            EDIT_SETTING,
            RELOAD_SETTING,
        ];
        let unique: std::collections::BTreeSet<&str> = id.iter().copied().collect();
        assert_eq!(unique.len(), id.len());
    }

    #[test]
    fn quit_is_the_only_id_that_exit() {
        // `muda` ids share one global stream, so an id this app never issued
        // must not be able to take the process down.
        assert_eq!(ask(&MenuId::new("splaude:something-else")), Ask::Ignore);
        assert_eq!(ask(&MenuId::new("")), Ask::Ignore);
    }
}
