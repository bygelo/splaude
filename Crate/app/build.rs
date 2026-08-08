// Put the splaude mark on `splaude.exe` itself.
//
// Without this the binary carries the default Rust executable icon in Explorer,
// on the taskbar and in Alt-Tab, while the tray — three feet away on the same
// screen — draws the real mark. The two are the same drawing here: `src/icon.rs`
// is `include!`d rather than copied, so there is no second renderer to drift.
// That drift is not hypothetical; it is exactly what happened to the macOS
// `.icns`, and `Script/makeicon.swift` exists to undo it.
//
// A build script cannot depend on its own crate, which is why `src/icon.rs`
// references nothing but `std`. See the header there before adding a `use` to
// it — including a file that names `tray_icon` or `anyhow` breaks this script,
// and the failure reads as if the icon module were at fault.

// `#[allow(dead_code)]` because the `.ico` only wants the idle mark, so the
// meter half of the module is unused here — and on a non-Windows target none of
// it is used at all. `cargo clippy --all-targets -- -D warnings` lints build
// scripts too, so an unadorned `include!` fails the Linux and macOS CI legs.
#[allow(dead_code)]
mod icon {
    include!("src/icon.rs");
}

/// The sizes Windows actually asks an `.ico` for: the tray and small list views
/// take 16, the taskbar and Alt-Tab 32 (64 at 200% scaling), Explorer's medium
/// and large views 48 and 128, and the extra-large view and the Vista+ shell
/// preview 256. Each is drawn from scratch rather than downscaled off one
/// bitmap — the renderer supersamples, and asking it for the size beats
/// resampling a 256px master into a 16px one.
const SIZE: [u32; 6] = [16, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");

    // `CARGO_CFG_TARGET_OS` and not `cfg!(windows)`: a build script is compiled
    // for and run on the *host*, so `cfg!` would answer for the machine doing
    // the building rather than the machine that will run the binary. On a Linux
    // or macOS leg this returns and the script is a no-op.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR for a build script");
    // Into `OUT_DIR`, never into the tree: a generated `.ico` committed to git
    // is a second copy of the mark, and a second copy is the thing this whole
    // arrangement exists to prevent.
    let path = std::path::Path::new(&out_dir).join("splaude.ico");

    let ico = ico(&SIZE);
    std::fs::write(&path, ico).expect("could not write the generated icon");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&path.to_string_lossy());
    // winresource writes a VERSIONINFO block whether or not it is asked to, and
    // its defaults come from `CARGO_PKG_*` — which would label the binary
    // "splaude-app", the crate name, rather than "splaude", the `[[bin]]` name
    // and the only name a user ever sees. Cheaper to say it than to ship a
    // properties dialog that disagrees with the icon next to it.
    resource.set("ProductName", "splaude");
    resource.set("FileDescription", "splaude — push-to-talk dictation");
    resource.set("OriginalFilename", "splaude.exe");

    // A failure here is a build failure on purpose. The alternative — warn and
    // carry on — ships the default Rust icon again, which is the bug, and does
    // it silently.
    resource
        .compile()
        .expect("could not embed the icon resource");
}

/// The `.ico` container, written by hand.
///
/// Deliberately no image crate: `.ico` is a six-byte header, a sixteen-byte
/// directory entry per image, and the payloads, and the payload format that
/// needs no encoder at all is a bare DIB. A PNG payload would be a tenth of the
/// size, but PNG means deflate, and deflate means either a real compressor
/// written here or `png`/`flate2` on the build graph. The trade bought is
/// roughly 350 KB of resource against zero encoding dependencies, in a binary
/// already measured in megabytes.
fn ico(size: &[u32]) -> Vec<u8> {
    let image: Vec<Vec<u8>> = size.iter().map(|&edge| dib(edge)).collect();

    let mut out = Vec::new();
    // ICONDIR: reserved, type 1 (icon, as opposed to 2 for a cursor), count.
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(size.len() as u16).to_le_bytes());

    // The directory is fixed-width, so every payload offset is known before a
    // single byte of pixel data is appended.
    let mut offset = 6 + 16 * size.len() as u32;
    for (&edge, payload) in size.iter().zip(&image) {
        // ICONDIRENTRY. Width and height are single bytes, which is why 256 —
        // the largest size the format can name — is written as 0.
        let dimension = u8::try_from(edge).unwrap_or(0);
        out.push(dimension);
        out.push(dimension);
        out.push(0); // Palette size; zero for a true-colour image.
        out.push(0); // Reserved.
        out.extend_from_slice(&1u16.to_le_bytes()); // Colour planes.
        out.extend_from_slice(&32u16.to_le_bytes()); // Bits per pixel.
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += payload.len() as u32;
    }

    for payload in image {
        out.extend_from_slice(&payload);
    }
    out
}

/// One image as a 32-bit bottom-up DIB, which is what an `.ico` payload is when
/// it is not a PNG: a `BITMAPINFOHEADER`, the BGRA pixels, and an AND mask.
fn dib(edge: u32) -> Vec<u8> {
    let pixel = icon::render(icon::Mood::Idle, edge);

    // The AND mask is one bit per pixel with rows padded to four bytes. On a
    // 32-bit icon Windows takes transparency from the alpha channel and ignores
    // it, but the format still requires the bytes and some tooling still reads
    // the height as evidence the mask is there — hence the doubled `biHeight`
    // below, which is the format's way of saying "colour rows then mask rows".
    let mask_stride = (edge.div_ceil(32) * 4) as usize;
    let mask_len = mask_stride * edge as usize;

    let mut out = Vec::with_capacity(40 + pixel.len() + mask_len);

    // BITMAPINFOHEADER.
    out.extend_from_slice(&40u32.to_le_bytes()); // Header size.
    out.extend_from_slice(&(edge as i32).to_le_bytes()); // biWidth.
    out.extend_from_slice(&((edge * 2) as i32).to_le_bytes()); // biHeight: colour + mask.
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes.
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount.
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression: BI_RGB.
    out.extend_from_slice(&((pixel.len() + mask_len) as u32).to_le_bytes()); // biSizeImage.
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter.
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter.
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed.
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant.

    // A DIB is stored bottom row first, and its channel order is BGRA where the
    // renderer produces RGBA. Alpha stays straight — an icon is not
    // premultiplied, and premultiplying here would darken every antialiased
    // edge against a light background.
    for y in (0..edge).rev() {
        let row = (y * edge * 4) as usize;
        for x in 0..edge as usize {
            let slot = row + x * 4;
            out.push(pixel[slot + 2]);
            out.push(pixel[slot + 1]);
            out.push(pixel[slot]);
            out.push(pixel[slot + 3]);
        }
    }

    out.resize(out.len() + mask_len, 0);
    out
}
