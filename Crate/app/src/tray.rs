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
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

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

/// Namespaced because `muda` ids share one global event stream.
const QUIT: &str = "splaude:quit";

/// What the icon is currently saying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    Idle,
    Recording,
}

impl Mood {
    /// The Swift app leaves its status item a template image and tints it red
    /// while recording. There is no template image on Windows, so idle is drawn
    /// near-white over a dark halo instead — the halo is what keeps it legible
    /// on a light taskbar.
    fn fill(self) -> [u8; 3] {
        match self {
            Mood::Idle => [0xF2, 0xF4, 0xF7],
            Mood::Recording => [0xFF, 0x3B, 0x30],
        }
    }
}

/// Drawn under the mark, never on its own.
const HALO: [u8; 3] = [0x1B, 0x1E, 0x24];

pub struct Tray {
    icon: TrayIcon,
    mood: Mood,
    hotkey: String,
}

impl Tray {
    /// Create the icon. Call this once the loop is already running rather than
    /// before it: `tray-icon` documents that a macOS status item built before
    /// the run loop starts misbehaves around fullscreen apps.
    pub fn new(hotkey: &str) -> Result<Self> {
        let menu = Menu::new();

        // Disabled on purpose — it is a label, and an enabled item that does
        // nothing when clicked reads as broken.
        append(
            &menu,
            &MenuItem::new(format!("Hold {hotkey} to dictate"), false, None),
        )?;
        append(&menu, &PredefinedMenuItem::separator())?;
        append(&menu, &MenuItem::with_id(QUIT, "Quit splaude", true, None))?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(Mood::Idle, hotkey))
            .with_icon(image(Mood::Idle)?)
            .build()
            .map_err(|error| anyhow!("could not create the tray icon: {error}"))?;

        // Nothing here wants click events, but an unread `TrayIconEvent` is not
        // discarded — it is queued on an unbounded channel forever, and `Move`
        // fires for every pixel the pointer crosses. Swallow them at the source.
        TrayIconEvent::set_event_handler(Some(|_| {}));

        announce();

        Ok(Self {
            icon,
            mood: Mood::Idle,
            hotkey: hotkey.to_string(),
        })
    }

    /// Reflect the take lifecycle. Cheap to call on every edge — a repeat is
    /// dropped before it reaches the platform.
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
}

/// Whether a menu event asks the app to exit.
pub fn is_quit(id: &MenuId) -> bool {
    id == QUIT
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
        Mood::Recording => "splaude — recording".to_string(),
    }
}

fn image(mood: Mood) -> Result<Icon> {
    Icon::from_rgba(rgba(mood), EDGE, EDGE)
        .map_err(|error| anyhow!("could not build the tray image: {error}"))
}

/// The icon as straight (non-premultiplied) RGBA, row-major, which is what
/// `Icon::from_rgba` wants.
fn rgba(mood: Mood) -> Vec<u8> {
    let fill = mood.fill();
    let mut pixel = Vec::with_capacity((EDGE * EDGE * 4) as usize);

    for y in 0..EDGE {
        for x in 0..EDGE {
            let front = coverage(x, y, in_mark);
            let behind = coverage(x, y, in_halo) * (1.0 - front);
            let alpha = front + behind;

            if alpha <= f32::EPSILON {
                pixel.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            for channel in 0..3 {
                let blended =
                    (f32::from(fill[channel]) * front + f32::from(HALO[channel]) * behind) / alpha;
                pixel.push(blended.round().clamp(0.0, 255.0) as u8);
            }
            pixel.push((alpha * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }

    pixel
}

/// How much of one pixel the shape covers, by supersampling.
fn coverage(x: u32, y: u32, shape: fn(f32, f32) -> bool) -> f32 {
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

/// A microphone: capsule, cradle, stem, base. Coordinates are in the 32-pixel
/// square and nothing reaches the edge, so the mark keeps its margin at every
/// size the platform rescales it to.
fn in_mark(x: f32, y: f32) -> bool {
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

/// The mark grown by about a pixel, which is what gets drawn dark underneath.
fn in_halo(x: f32, y: f32) -> bool {
    const REACH: f32 = 1.1;
    const AROUND: u32 = 8;

    in_mark(x, y)
        || (0..AROUND).any(|step| {
            let angle = std::f32::consts::TAU * step as f32 / AROUND as f32;
            in_mark(x + REACH * angle.cos(), y + REACH * angle.sin())
        })
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

    // No test here builds a real tray icon: CI has no desktop session.

    #[test]
    fn image_is_a_square_rgba_buffer() {
        for mood in [Mood::Idle, Mood::Recording] {
            assert_eq!(rgba(mood).len(), (EDGE * EDGE * 4) as usize);
        }
    }

    #[test]
    fn the_two_mood_differ() {
        assert_ne!(rgba(Mood::Idle), rgba(Mood::Recording));
    }

    #[test]
    fn recording_is_the_red_one() {
        let center = ((EDGE / 2 * EDGE) + EDGE / 2) as usize * 4;
        let idle = rgba(Mood::Idle);
        let recording = rgba(Mood::Recording);

        let redness = |pixel: &[u8]| i32::from(pixel[center]) - i32::from(pixel[center + 1]);
        assert!(redness(&recording) > redness(&idle) + 100);
    }

    #[test]
    fn the_mark_is_drawn_and_the_corner_are_not() {
        let pixel = rgba(Mood::Idle);
        let alpha: Vec<u8> = pixel.iter().skip(3).step_by(4).copied().collect();

        // Something opaque in the middle.
        assert!(alpha.contains(&255));
        // The corners are the margin the mark is supposed to keep.
        for corner in [0, (EDGE - 1) as usize, (EDGE * (EDGE - 1)) as usize] {
            assert_eq!(alpha[corner], 0);
        }
    }

    #[test]
    fn the_halo_surrounds_the_mark() {
        // Every mark pixel is a halo pixel, and the halo is strictly larger —
        // otherwise the idle mark has no contrast on a light taskbar.
        let mut mark = 0;
        let mut halo = 0;
        for y in 0..EDGE {
            for x in 0..EDGE {
                let inside = in_mark(x as f32 + 0.5, y as f32 + 0.5);
                if inside {
                    mark += 1;
                    assert!(in_halo(x as f32 + 0.5, y as f32 + 0.5));
                }
                if in_halo(x as f32 + 0.5, y as f32 + 0.5) {
                    halo += 1;
                }
            }
        }
        assert!(mark > 0);
        assert!(halo > mark);
    }

    #[test]
    fn quit_is_the_only_id_that_exit() {
        assert!(is_quit(&MenuId::new(QUIT)));
        assert!(!is_quit(&MenuId::new("splaude:something-else")));
    }
}
