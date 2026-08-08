// The splaude mark, drawn in pure pixel math.
//
// This file is the single renderer of the mark on Windows. `tray.rs` calls it
// for the status-bar image; `build.rs` `include!`s it to render the `.ico` that
// gets embedded in `splaude.exe`, so Explorer, the taskbar and Alt-Tab show the
// same mark the tray does. Keeping one renderer is the whole point: the shipped
// macOS `.icns` drifted from the mark precisely because a human regenerated it
// by hand, which is why `Script/makeicon.swift` exists at all.
//
// Two rules follow from the `include!`, and both look like style nits until you
// break one:
//
//   * Nothing here may reference anything but `std`. A build script cannot
//     depend on its own crate, so `tray_icon`, `anyhow` and every other name in
//     `Cargo.toml`'s `[dependencies]` are unavailable at the point this text is
//     compiled into `build.rs`.
//   * The header above is `//` and not `//!` on purpose. `include!` expands to
//     items, and an inner doc comment cannot be produced by macro expansion —
//     rustc rejects the file with E0753 the moment `build.rs` pulls it in.
//     Promoting these lines to a module doc comment breaks the Windows build.

/// Square edge of the generated icon, in pixels. Both platforms scale from
/// whatever they are given; 32 is small enough to stay cheap and large enough
/// that the cradle arc does not collapse into the capsule.
pub const EDGE: u32 = 32;

/// Samples per axis when rasterising. Plain supersampling — at this size it is
/// a few thousand predicate calls once per state change, and it is the whole
/// difference between a microphone and a smudge.
const SAMPLE: u32 = 4;

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

/// The icon as straight (non-premultiplied) RGBA, row-major, which is what
/// `Icon::from_rgba` wants.
pub fn rgba(mood: Mood) -> Vec<u8> {
    render(mood, EDGE)
}

/// Draw the mark at an arbitrary edge. Parameterised rather than hard-wired to
/// [`EDGE`] so the mark can be checked at the size Windows actually paints it
/// at — every proportion here is a fraction of the canvas, and a design that
/// only holds together at one size is not a design. `build.rs` uses the same
/// lever to ask for each size the `.ico` carries, rather than scaling one
/// bitmap down and losing the supersampling.
pub fn render(mood: Mood, edge: u32) -> Vec<u8> {
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
    fn every_size_the_ico_carries_draws_the_mark() {
        // `build.rs` asks for exactly these, and an empty buffer would embed a
        // transparent icon that Explorer renders as nothing at all — which is
        // indistinguishable from the default-icon bug this replaces.
        for edge in [16u32, 32, 48, 64, 128, 256] {
            let pixel = render(Mood::Idle, edge);
            assert_eq!(pixel.len(), (edge * edge * 4) as usize);

            let opaque = pixel.chunks_exact(4).filter(|slot| slot[3] == 255).count();
            let white = pixel
                .chunks_exact(4)
                .filter(|slot| slot[3] == 255 && slot[..3].iter().all(|&value| value > 200))
                .count();

            // The face covers most of the canvas and the mic is a visible share
            // of it at every size Windows asks for.
            assert!(
                opaque * 2 > (edge * edge) as usize,
                "{edge}px is mostly transparent"
            );
            assert!(white > 0, "no mic at {edge}px");
        }
    }
}
