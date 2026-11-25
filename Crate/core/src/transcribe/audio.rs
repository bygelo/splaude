//! Getting 16 kHz mono PCM out of whatever the user has — a video, a voice
//! memo, a podcast, a URL ffmpeg can read.
//!
//! ffmpeg rather than a decoding crate: a video file is a container problem
//! (mp4, mkv, mov, webm, avi…) on top of a codec problem (AAC, Opus, AC-3,
//! MP3…), and no pure-Rust stack covers what a friend's phone or screen
//! recorder produces. Every machine that edits video already has ffmpeg, and
//! the one that does not gets a one-line install hint.

use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// The format every backend here wants.
pub const SAMPLE_RATE: u32 = 16_000;

fn install_hint() -> &'static str {
    if cfg!(target_os = "windows") {
        "install it with `winget install Gyan.FFmpeg`, then open a new terminal"
    } else if cfg!(target_os = "macos") {
        "install it with `brew install ffmpeg`"
    } else {
        "install it with your package manager (e.g. `sudo apt install ffmpeg`)"
    }
}

/// Decodes the first audio track of `input` to 16 kHz mono signed 16-bit.
pub fn decode(input: &str) -> Result<Vec<i16>> {
    let mut child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-vn", "-map", "0:a:0", "-ac", "1", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-f", "s16le", "-acodec", "pcm_s16le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("ffmpeg was not found on PATH — {}", install_hint())
            } else {
                anyhow::anyhow!("could not start ffmpeg: {error}")
            }
        })?;

    let mut byte = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_end(&mut byte)
        .context("reading decoded audio from ffmpeg")?;
    let mut complaint = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut complaint);
    }
    let status = child.wait()?;

    if !status.success() {
        let complaint = complaint.trim();
        if complaint.contains("matches no streams") {
            bail!("{input} has no audio track");
        }
        bail!(
            "ffmpeg could not decode {input}: {}",
            if complaint.is_empty() {
                "no detail"
            } else {
                complaint
            }
        );
    }
    if byte.len() < 2 {
        bail!("{input} decoded to no audio at all");
    }

    Ok(byte
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

/// Little-endian bytes, the shape the socket takes.
pub fn to_byte(sample: &[i16]) -> Vec<u8> {
    sample
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub fn duration_ms(sample: &[i16]) -> u64 {
    sample.len() as u64 * 1000 / SAMPLE_RATE as u64
}

pub fn sample_at(ms: u64) -> usize {
    (ms * SAMPLE_RATE as u64 / 1000) as usize
}
