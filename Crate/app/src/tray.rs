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
//! The icon is drawn here rather than shipped as a file: an image asset means
//! an image decoder in the dependency tree, and `Resource/splaude.icns` is
//! macOS-only anyway.

use anyhow::{anyhow, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use splaude_core::diagnostic;

/// Re-exported so `main.rs` can name it without depending on `tray-icon`
/// directly — the whole point being that on Linux neither exists.
pub use tray_icon::menu::MenuId;

/// Square edge of the generated icon, in pixels. Both platforms scale from
/// whatever they are given; 32 is small enough to stay cheap and large enough
/// that the cradle arc does not collapse into the capsule.
const EDGE: u32 = 32;

/// Samples per axis when rasterising. Plain supersampling — at this size it is
/// a few thousand predicate calls once per state change, and it is the whole
/// difference between a microphone and a smudge.
const SAMPLE: u32 = 4;

// Namespaced because `muda` ids share one global event stream, and constant
// across menu rebuilds so a click always means the same thing.
const QUIT: &str = "splaude:quit";
const TRANSCRIPT: &str = "splaude:transcript";
const REVEAL_LOG: &str = "splaude:reveal-log";
const AUTOSTART: &str = "splaude:autostart";

/// Longest transcript preview the menu will show, in characters.
///
/// A menu is as wide as its widest item, so a sentence of dictation dropped in
/// untruncated stretches the whole thing across the screen. The Swift build
/// dodged this with a fixed "Copy Last Transcript" title and the text in a
/// tooltip; showing a clipped preview is the same trade with the words visible.
const PREVIEW_LIMIT: usize = 40;

// Proportions of the app mark, lifted from `Script/makeicon.swift`, which
// renders the shipped `.icns`. They are fractions of the canvas rather than
// pixel counts precisely because that file emits ten sizes off the same grid;
// matching them is what makes this icon and the macOS one the same mark.

/// Margin on every side. This is the second number that departs from
/// `makeicon.swift`, which uses 100/1024, and it departs because the platforms
/// disagree about who owns the padding. A macOS Dock icon is expected to bring
/// its own — the Dock lays out the bitmap edge to edge and the art insets
/// itself. Windows does the reverse: it spaces tray icons for you and expects
/// the bitmap filled, so carrying the Dock margin across renders splaude at
/// roughly four fifths of its neighbours and it reads as a small mark rather
/// than a small icon. Only enough is kept here to stop the antialiased edge
/// clipping against the bitmap.
const INSET: f32 = 8.0 / 1024.0;

/// Corner radius, as a fraction of the rounded square's own width (824/1024).
const CORNER: f32 = 185.0 / 824.0;

/// Height of the mic glyph over the face. This is the one number that departs
/// from `makeicon.swift`, which uses 430/1024 — sized for an `.icns` whose
/// smallest rendering is still a 16pt retina pair. Windows draws the tray icon
/// at 16 physical pixels, and at 430/1024 that leaves a seven-pixel mic whose
/// cradle, stem and base all merge into one white block. Growing the glyph is
/// the only lever that survives the downscale; the mark reads as splaude with a
/// larger mic, and reads as nothing at all with an illegible one.
const GLYPH: f32 = 600.0 / 1024.0;

/// A mic carries more visual mass in the capsule than in the stand, so a true
/// centre reads high. Nudge it down.
const OPTICAL_DROP: f32 = 0.005;

/// Top-of-face brightness. A shallow lift keeps the face from reading as flat
/// vinyl when the platform scales up, without becoming a visible band at 16px.
const LIFT: f32 = 1.12;

/// How many levels the recording icon can show, silence included.
pub const STEP_COUNT: u8 = 5;

/// Quantise a smoothed 0…1 input level onto [`STEP_COUNT`] steps.
///
/// The audio thread produces a level per buffer — hundreds of times a second —
/// and every distinct step costs a fresh rasterisation and a platform icon
/// swap, so the meter is deliberately coarse. Five steps is enough that speech
/// visibly moves the icon and few enough that ordinary speech redraws it a
/// handful of times a second rather than continuously. Caller-side dedup does
/// the rest: only a *change* of step is worth pushing.
pub fn step_of(level: f32) -> u8 {
    // Saturating cast, so a NaN level lands on silence rather than panicking.
    let step = (level.clamp(0.0, 1.0) * f32::from(STEP_COUNT)) as u8;
    step.min(STEP_COUNT - 1)
}

/// What the icon is currently saying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    Idle,
    /// Recording, carrying the input level as a [`step_of`] step. The Swift
    /// build swapped between two SF Symbols for the same purpose; here the
    /// face doubles as the meter, so a dead microphone is visible at a glance
    /// instead of after a silent failed take.
    Recording(u8),
}

impl Mood {
    /// The colour of the rounded square where the meter has reached. Idle is
    /// the app tint (the `TINT` default in the `Makefile`); recording is the
    /// same red the Swift app tints its menu bar item with. Swapping only the
    /// face means the icon still reads as splaude while the state is
    /// unmistakable.
    fn face(self) -> [u8; 3] {
        match self {
            Mood::Idle => [0xD9, 0x77, 0x57],
            Mood::Recording(_) => [0xFF, 0x3B, 0x30],
        }
    }

    /// The colour above the meter. Darker than [`Mood::face`] but no less
    /// saturated: a silent take still has to read as *recording* first and as
    /// *quiet* second, and desaturating is what would cost that.
    fn quiet(self) -> [u8; 3] {
        match self {
            Mood::Idle => self.face(),
            Mood::Recording(_) => [0xA8, 0x18, 0x12],
        }
    }

    /// How much of the square, measured up from its bottom edge, is painted at
    /// [`Mood::face`]. Idle fills completely, which collapses to the flat tint
    /// the mark has always had.
    fn fill(self) -> f32 {
        match self {
            Mood::Idle => 1.0,
            Mood::Recording(step) => {
                f32::from(step.min(STEP_COUNT - 1)) / f32::from(STEP_COUNT - 1)
            }
        }
    }
}

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
    /// A click on a label, or on an id from some other `muda` consumer.
    Ignore,
}

pub struct Tray {
    icon: TrayIcon,
    mood: Mood,
    hotkey: String,
    /// The credential sentence, or `None` when there is nothing worth saying.
    health: Option<String>,
    /// The last take's committed text in full. The menu shows a clipped
    /// preview; the clipboard gets all of it.
    transcript: Option<String>,
    autostart: bool,
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
            transcript: None,
            autostart,
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
            &MenuItem::new(format!("Hold {} to dictate", self.hotkey), false, None),
        )?;
        append(&menu, &PredefinedMenuItem::separator())?;

        append(
            &menu,
            &CheckMenuItem::with_id(AUTOSTART, "Launch at login", true, self.autostart, None),
        )?;
        append(
            &menu,
            &MenuItem::with_id(REVEAL_LOG, "Reveal Log", true, None),
        )?;
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
        _ => Ask::Ignore,
    }
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
    Icon::from_rgba(rgba(mood), EDGE, EDGE)
        .map_err(|error| anyhow!("could not build the tray image: {error}"))
}

/// The icon as straight (non-premultiplied) RGBA, row-major, which is what
/// `Icon::from_rgba` wants.
fn rgba(mood: Mood) -> Vec<u8> {
    render(mood, EDGE)
}

/// Draw the mark at an arbitrary edge. Parameterised rather than hard-wired to
/// [`EDGE`] so the mark can be checked at the size Windows actually paints it
/// at — every proportion here is a fraction of the canvas, and a design that
/// only holds together at one size is not a design.
fn render(mood: Mood, edge: u32) -> Vec<u8> {
    let size = edge as f32;
    let inset = size * INSET;
    let span = size - inset * 2.0;
    // The meter fills the face from the bottom up, the way a level meter does,
    // so everything at or below this line is painted at the loud colour and
    // everything above it at the quiet one. Idle fills the square completely,
    // which is the same flat tint the mark has always had.
    let waterline = size - inset - span * mood.fill();
    let mut pixel = Vec::with_capacity((edge * edge * 4) as usize);

    for y in 0..edge {
        let face = if y as f32 + 0.5 >= waterline {
            mood.face()
        } else {
            mood.quiet()
        };

        for x in 0..edge {
            let glyph = coverage(x, y, |at_x, at_y| in_glyph(at_x, at_y, size));
            let plate = coverage(x, y, |at_x, at_y| in_plate(at_x, at_y, size)) * (1.0 - glyph);
            let alpha = glyph + plate;

            if alpha <= f32::EPSILON {
                pixel.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            // Gradient sampled at the pixel centre: full lift at the top of the
            // square easing to the flat tint at the bottom.
            let down = ((y as f32 + 0.5 - inset) / span).clamp(0.0, 1.0);
            let brightness = LIFT + (1.0 - LIFT) * down;

            for channel in face {
                let tinted = (f32::from(channel) * brightness).min(255.0);
                let blended = (255.0 * glyph + tinted * plate) / alpha;
                pixel.push(blended.round().clamp(0.0, 255.0) as u8);
            }
            pixel.push((alpha * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }

    pixel
}

/// How much of one pixel the shape covers, by supersampling.
fn coverage(x: u32, y: u32, shape: impl Fn(f32, f32) -> bool) -> f32 {
    let step = 1.0 / SAMPLE as f32;
    let mut hit = 0u32;

    for sub_y in 0..SAMPLE {
        for sub_x in 0..SAMPLE {
            let at_x = x as f32 + (sub_x as f32 + 0.5) * step;
            let at_y = y as f32 + (sub_y as f32 + 0.5) * step;
            if shape(at_x, at_y) {
                hit += 1;
            }
        }
    }

    hit as f32 / (SAMPLE * SAMPLE) as f32
}

/// The tinted rounded square the whole mark sits on.
fn in_plate(x: f32, y: f32, size: f32) -> bool {
    let inset = size * INSET;
    let far = size - inset;
    rounded(x, y, inset, inset, far, far, (far - inset) * CORNER)
}

/// The white mic, placed on the face. [`in_mic`] draws in its own 32-unit box,
/// so this is just the map from canvas coordinates onto that box: scale the
/// glyph to [`GLYPH`] of the canvas, then centre it optically.
fn in_glyph(x: f32, y: f32, size: f32) -> bool {
    /// Height and centre of [`in_mic`]'s drawing, in its own units.
    const MIC_HEIGHT: f32 = 24.5;
    const MIC_CENTER_X: f32 = 16.0;
    const MIC_CENTER_Y: f32 = 16.25;

    let scale = size * GLYPH / MIC_HEIGHT;
    let at_x = (x - size / 2.0) / scale + MIC_CENTER_X;
    let at_y = (y - (size / 2.0 + size * OPTICAL_DROP)) / scale + MIC_CENTER_Y;
    in_mic(at_x, at_y)
}

/// A microphone: capsule, cradle, stem, base, in a 32-unit box spanning
/// y 4.0..28.5 — the numbers [`in_glyph`] scales against.
fn in_mic(x: f32, y: f32) -> bool {
    // Capsule.
    rounded(x, y, 11.0, 4.0, 21.0, 19.0, 5.0)
        // Stem, from inside the cradle down to the base.
        || rounded(x, y, 14.75, 21.5, 17.25, 27.0, 1.0)
        // Base.
        || rounded(x, y, 10.5, 26.0, 21.5, 28.5, 1.25)
        || in_cradle(x, y)
}

/// The open arc under the capsule — the lower half of an annulus.
fn in_cradle(x: f32, y: f32) -> bool {
    let from_x = x - 16.0;
    let from_y = y - 13.0;
    let distance = from_x.mul_add(from_x, from_y * from_y).sqrt();
    from_y >= 0.0 && (8.6..10.6).contains(&distance)
}

/// Whether a point is inside an axis-aligned rectangle with rounded corners.
fn rounded(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }

    // Corner test only bites outside the inset box; inside it the clamp is a
    // no-op and the distance is zero.
    let near_x = x - x.clamp(x0 + radius, x1 - radius);
    let near_y = y - y.clamp(y0 + radius, y1 - radius);
    near_x.mul_add(near_x, near_y * near_y) <= radius * radius
}

#[cfg(test)]
mod test {
    use super::*;

    // No test here builds a real tray icon, opens a clipboard or spawns a file
    // manager: CI has no desktop session.

    /// The loudest step, i.e. a fully filled meter.
    const LOUD: Mood = Mood::Recording(STEP_COUNT - 1);

    /// Silence during a take — recording, meter empty.
    const HUSH: Mood = Mood::Recording(0);

    #[test]
    fn image_is_a_square_rgba_buffer() {
        for mood in [Mood::Idle, HUSH, LOUD] {
            assert_eq!(rgba(mood).len(), (EDGE * EDGE * 4) as usize);
        }
    }

    #[test]
    fn the_two_mood_differ() {
        assert_ne!(rgba(Mood::Idle), rgba(LOUD));
    }

    /// A pixel on the bare face: inside the square, left of the glyph.
    const ON_FACE: (u32, u32) = (7, EDGE / 2);

    /// A pixel in the mic capsule, which is white in both moods.
    const ON_GLYPH: (u32, u32) = (EDGE / 2, EDGE / 2);

    fn sample(pixel: &[u8], at: (u32, u32)) -> [u8; 4] {
        let start = (at.1 * EDGE + at.0) as usize * 4;
        pixel[start..start + 4].try_into().expect("four channels")
    }

    #[test]
    fn the_idle_face_is_the_app_tint() {
        // The one thing a future edit could silently break: the square is the
        // mark's whole identity, so pin it to the tint the `.icns` uses. The
        // window allows the gradient's lift without admitting white or red.
        let face = sample(&rgba(Mood::Idle), ON_FACE);
        assert_eq!(face[3], 255);
        for (channel, tint) in Mood::Idle.face().into_iter().enumerate() {
            let value = f32::from(face[channel]);
            assert!(
                value >= f32::from(tint) - 4.0 && value <= f32::from(tint) * LIFT + 4.0,
                "channel {channel} is {value}, not the tint {tint}"
            );
        }
    }

    #[test]
    fn the_glyph_is_white_in_every_mood() {
        // The other half of the same guarantee — a mic tinted like its face is
        // an invisible mic, and the meter must not eat it either.
        for mood in [Mood::Idle, HUSH, LOUD] {
            assert_eq!(sample(&rgba(mood), ON_GLYPH), [255, 255, 255, 255]);
        }
    }

    #[test]
    fn recording_swaps_the_face_and_nothing_else() {
        let idle = rgba(Mood::Idle);
        let recording = rgba(LOUD);

        // Same mark, different face. If the geometry moved, the icon would
        // change shape when a take starts instead of just changing colour, and
        // it would stop reading as splaude at the moment it matters most.
        let alpha = |pixel: &[u8]| {
            pixel
                .iter()
                .skip(3)
                .step_by(4)
                .copied()
                .collect::<Vec<u8>>()
        };
        assert_eq!(alpha(&idle), alpha(&recording));

        // Measured on the face, not the centre — the glyph is white either way.
        // Terracotta is a warm colour, so the margin has to be wide enough to
        // mean "recording" rather than merely "orange".
        let redness = |pixel: &[u8]| {
            let face = sample(pixel, ON_FACE);
            i32::from(face[0]) - i32::from(face[1])
        };
        assert!(redness(&recording) >= redness(&idle) + 60);

        // And a *silent* take, where the meter is empty and the whole face is
        // the quiet colour, still has to read as recording first: the darker
        // red is darker, not less red.
        assert!(redness(&rgba(HUSH)) >= redness(&idle) + 40);
    }

    #[test]
    fn the_meter_fills_the_face_from_the_bottom() {
        // A step is only worth rasterising if it is visible, so every one of
        // them has to reach further up the square than the last.
        // Counted down one column of bare face, left of the glyph, which is
        // white in every mood. The loud red saturates the channel at 255; the
        // quiet one cannot reach 188 even at the top of the gradient, so the
        // threshold separates them without pinning either colour.
        let lit = |mood: Mood| {
            let pixel = rgba(mood);
            (0..EDGE)
                .filter(|&y| sample(&pixel, (ON_FACE.0, y))[0] >= 220)
                .count()
        };

        assert_eq!(lit(HUSH), 0, "silence should leave the meter empty");

        let mut previous = 0;
        for step in 1..STEP_COUNT {
            let filled = lit(Mood::Recording(step));
            assert!(
                filled > previous,
                "step {step} fills {filled} rows, no more than step {} at {previous}",
                step - 1
            );
            previous = filled;
        }
    }

    #[test]
    fn a_level_quantises_onto_the_step() {
        assert_eq!(step_of(0.0), 0);
        assert_eq!(step_of(1.0), STEP_COUNT - 1);

        // Every step has to be reachable, or the meter has fewer than it says.
        let seen: std::collections::BTreeSet<u8> = (0..=100)
            .map(|hundredth| step_of(hundredth as f32 / 100.0))
            .collect();
        assert_eq!(seen.len(), usize::from(STEP_COUNT));

        // Monotonic, so a louder voice never shows a shorter bar.
        let mut previous = 0;
        for hundredth in 0..=100 {
            let step = step_of(hundredth as f32 / 100.0);
            assert!(step >= previous);
            previous = step;
        }
    }

    #[test]
    fn a_level_outside_the_range_does_not_escape_the_step() {
        // The level comes off the audio thread; nothing there guarantees the
        // range, and an out-of-range step would index a brighter red than the
        // icon has.
        for wild in [-1.0, -0.0, 1.5, f32::INFINITY, f32::NAN] {
            assert!(step_of(wild) < STEP_COUNT, "{wild} escaped the range");
        }
    }

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
    fn the_mark_is_drawn_and_the_corner_are_not() {
        let pixel = rgba(Mood::Idle);
        let alpha: Vec<u8> = pixel.iter().skip(3).step_by(4).copied().collect();

        // Something opaque in the middle.
        assert!(alpha.contains(&255));
        // The rounded square keeps its margin, so the bitmap corners stay clear
        // — both the inset and the corner radius are outside them.
        for corner in [
            0,
            (EDGE - 1) as usize,
            (EDGE * (EDGE - 1)) as usize,
            (EDGE * EDGE - 1) as usize,
        ] {
            assert_eq!(alpha[corner], 0);
        }
    }

    #[test]
    fn the_mic_survives_the_tray_size() {
        // Windows paints this at 16 physical pixels. The glyph has to keep a
        // readable share of the face there rather than dissolve into it —
        // shrink it and this is the count that drops first.
        let white = render(Mood::Idle, 16)
            .chunks_exact(4)
            .filter(|channel| channel.iter().all(|&value| value > 200))
            .count();
        assert!(white >= 20, "only {white} white pixel at 16px");
    }

    #[test]
    fn the_glyph_sits_inside_the_face() {
        // If the mic ever outgrew the square it would clip against the corners
        // instead of floating on the tint.
        for size in [16.0, 32.0, 1024.0] {
            let step = size / 256.0;
            let mut at_y = 0.0;
            while at_y < size {
                let mut at_x = 0.0;
                while at_x < size {
                    assert!(
                        !in_glyph(at_x, at_y, size) || in_plate(at_x, at_y, size),
                        "glyph escapes the face at {at_x},{at_y} of {size}"
                    );
                    at_x += step;
                }
                at_y += step;
            }
        }
    }

    #[test]
    fn every_actionable_id_maps_to_its_own_ask() {
        assert_eq!(ask(&MenuId::new(QUIT)), Ask::Quit);
        assert_eq!(ask(&MenuId::new(TRANSCRIPT)), Ask::CopyTranscript);
        assert_eq!(ask(&MenuId::new(REVEAL_LOG)), Ask::RevealLog);
        assert_eq!(ask(&MenuId::new(AUTOSTART)), Ask::ToggleAutostart);
    }

    #[test]
    fn quit_is_the_only_id_that_exit() {
        // `muda` ids share one global stream, so an id this app never issued
        // must not be able to take the process down.
        assert_eq!(ask(&MenuId::new("splaude:something-else")), Ask::Ignore);
        assert_eq!(ask(&MenuId::new("")), Ask::Ignore);
    }
}
