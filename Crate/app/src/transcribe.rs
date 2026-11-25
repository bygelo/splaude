//! `splaude transcribe <file>` — a video or recording in, subtitles and a timed
//! transcript out. The work is in `splaude_core::transcribe`; this is argument
//! parsing, choosing a backend, and writing files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use splaude_core::speech::deepgram::DeepgramSpeechBackend;
use splaude_core::speech::{AnthropicSpeechBackend, SpeechBackend};
use splaude_core::transcribe::{self, audio, context, cue, BackendFactory, Progress, Request};
use splaude_core::Setting;

pub const USAGE: &str = "\
splaude transcribe — subtitles and a timed transcript from a video or recording

USAGE:
    splaude transcribe <FILE or URL> [OPTION]...

OPTIONS:
    -o, --out <PATH>        Where to write. A directory, or a path whose extension
                            is replaced per format. Default: next to the input.
    -f, --format <LIST>     Comma-separated: srt, vtt, txt, json, or all.
                            Default: srt,txt
    -c, --context <TEXT>    What the recording is about, or a list of names and
                            terms. Names, acronyms and jargon in it are used to
                            bias recognition. Repeatable.
        --context-file <P>  The same, read from a file (one term per line, or prose).
    -t, --term <TERM>       One term to bias towards. Repeatable.
    -l, --language <TAG>    Spoken language, e.g. en, es, ja. Default: the setting.
    -p, --parallel <N>      Pieces transcribed at once (1-5). Default: 4. Claude
                            accounts get 5 live speech sessions, and dictating
                            uses one; the run backs off if it meets the limit.
        --backend <NAME>    anthropic (Claude Code sign-in) or deepgram (your own
                            key in the settings file). Default: the setting.
        --stdout            Also print the text transcript.
    -v, --verbose           Show the socket log as it runs.

Needs ffmpeg on PATH. A URL is downloaded with yt-dlp when it is installed,
otherwise handed to ffmpeg as-is.

Timings come from feeding audio at real speed, so each piece takes as long as it
lasts; --parallel is what makes a long video quick. Word timings are typically
within a few tenths of a second; cue timings are what subtitles need.

EXAMPLE:
    splaude transcribe talk.mp4 -c \"Interview with Dr. Ngozi Okonkwo about CRISPR\"
";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    Srt,
    Vtt,
    Txt,
    Json,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Txt => "txt",
            Format::Json => "json",
        }
    }
}

struct Argument {
    input: String,
    out: Option<PathBuf>,
    format: Vec<Format>,
    context: Vec<String>,
    term: Vec<String>,
    language: Option<String>,
    parallel: usize,
    backend: Option<String>,
    stdout: bool,
    verbose: bool,
}

fn parse(argument: &[String]) -> Result<Argument> {
    let mut option = Argument {
        input: String::new(),
        out: None,
        format: vec![Format::Srt, Format::Txt],
        context: Vec::new(),
        term: Vec::new(),
        language: None,
        parallel: 4,
        backend: None,
        stdout: false,
        verbose: false,
    };
    let mut rest = argument.iter();
    while let Some(flag) = rest.next() {
        let mut value = |name: &str| -> Result<String> {
            rest.next()
                .cloned()
                .with_context(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-o" | "--out" => option.out = Some(PathBuf::from(value(flag)?)),
            "-f" | "--format" => {
                option.format = Vec::new();
                for name in value(flag)?
                    .split(',')
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                {
                    match name.to_lowercase().as_str() {
                        "srt" => option.format.push(Format::Srt),
                        "vtt" | "webvtt" => option.format.push(Format::Vtt),
                        "txt" | "text" => option.format.push(Format::Txt),
                        "json" => option.format.push(Format::Json),
                        "all" => {
                            option.format =
                                vec![Format::Srt, Format::Vtt, Format::Txt, Format::Json]
                        }
                        other => bail!("unknown format {other:?} — use srt, vtt, txt, json or all"),
                    }
                }
                option.format.dedup();
                if option.format.is_empty() {
                    bail!("--format needs at least one of srt, vtt, txt, json");
                }
            }
            "-c" | "--context" => option.context.push(value(flag)?),
            "--context-file" => {
                let path = value(flag)?;
                option.context.push(
                    std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
                );
            }
            "-t" | "--term" => option.term.push(value(flag)?),
            "-l" | "--language" => option.language = Some(value(flag)?),
            "-p" | "--parallel" => {
                option.parallel = value(flag)?
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=16).contains(n))
                    .context("--parallel takes a number from 1 to 16")?;
                // Over the account limit buys nothing but rejected sockets.
                // Deepgram has its own, higher limits, so only the default
                // backend is capped — after the backend is known, in `main`.
            }
            "--backend" => option.backend = Some(value(flag)?),
            "--stdout" => option.stdout = true,
            "-v" | "--verbose" => option.verbose = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with('-') && other.len() > 1 => bail!("unknown option {other}"),
            other => {
                if !option.input.is_empty() {
                    bail!("one input at a time — got {:?} and {other:?}", option.input);
                }
                option.input = other.to_string();
            }
        }
    }
    if option.input.is_empty() {
        bail!("no input — give a video, audio file or URL");
    }
    Ok(option)
}

fn is_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// Fetches a page URL's audio with yt-dlp into `directory`, when yt-dlp exists.
/// Returns the downloaded file and the video's title.
fn download(url: &str, directory: &Path) -> Option<(PathBuf, String)> {
    let output = std::process::Command::new("yt-dlp")
        .args([
            "--no-playlist",
            "-f",
            "bestaudio/best",
            "--print",
            "after_move:filepath",
            "--print",
            "title",
            "-o",
        ])
        .arg(directory.join("%(id)s.%(ext)s"))
        .arg(url)
        .stderr(std::process::Stdio::inherit())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut line = text.lines().filter(|l| !l.trim().is_empty());
    let title = line.next()?.trim().to_string();
    let path = PathBuf::from(line.next()?.trim());
    path.exists().then_some((path, title))
}

fn safe_name(title: &str) -> String {
    let name: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim().trim_matches('.').to_string();
    if name.is_empty() {
        "transcript".into()
    } else {
        name.chars().take(80).collect()
    }
}

fn clock(ms: u64) -> String {
    let second = ms / 1000;
    format!(
        "{}:{:02}:{:02}",
        second / 3600,
        second / 60 % 60,
        second % 60
    )
}

fn factory(
    setting: &Setting,
    backend_name: &str,
    keyterm: Vec<String>,
    store: Arc<splaude_core::Store>,
) -> Result<BackendFactory> {
    match backend_name {
        "deepgram" => {
            // Fail now, not inside the first piece.
            DeepgramSpeechBackend::new(setting)?;
            let setting = setting.clone();
            Ok(Arc::new(move || {
                Ok(
                    Box::new(DeepgramSpeechBackend::new(&setting)?.with_keyterm(keyterm.clone()))
                        as Box<dyn SpeechBackend>,
                )
            }))
        }
        "anthropic" | "" => {
            store.load().context("no usable Claude Code credential")?;
            let setting = setting.clone();
            Ok(Arc::new(move || {
                let credential = store.load().context("no usable Claude Code credential")?;
                Ok(Box::new(
                    AnthropicSpeechBackend::new_bare(credential, &setting.language)
                        .with_keyterm(keyterm.clone())
                        .with_store(store.clone()),
                ) as Box<dyn SpeechBackend>)
            }))
        }
        other => bail!("unknown backend {other:?} — use anthropic or deepgram"),
    }
}

pub fn main(argument: &[String], store: splaude_core::Store) -> Result<()> {
    let option = parse(argument)?;
    if !option.verbose {
        splaude_core::diagnostic::quiet();
    }
    let (mut setting, _) = Setting::load_checked();
    if let Some(language) = &option.language {
        setting.language = language.clone();
    }

    // Context first so it outranks anything else for the limited slots.
    let mut keyterm: Vec<String> = option.term.clone();
    for text in &option.context {
        keyterm.extend(context::keyterm_of(text));
    }
    keyterm.extend(setting.custom_keyterm.iter().cloned());
    let keyterm = splaude_core::speech::anthropic::normalise_keyterm(&keyterm);

    let backend_name = option
        .backend
        .clone()
        .unwrap_or_else(|| setting.speech_backend.trim().to_string());
    let backend = factory(&setting, &backend_name, keyterm.clone(), Arc::new(store))?;
    let parallel = if backend_name == "deepgram" {
        option.parallel
    } else {
        option.parallel.min(transcribe::SESSION_LIMIT)
    };

    // Resolve the input: a URL goes through yt-dlp when it can.
    let scratch = std::env::temp_dir().join(format!("splaude-transcribe-{}", std::process::id()));
    let (source, default_base) = if is_url(&option.input) {
        std::fs::create_dir_all(&scratch)?;
        eprintln!("splaude: fetching {}", option.input);
        match download(&option.input, &scratch) {
            Some((path, title)) => (
                path.to_string_lossy().to_string(),
                std::env::current_dir()?.join(safe_name(&title)),
            ),
            None => (
                option.input.clone(),
                std::env::current_dir()?.join("transcript"),
            ),
        }
    } else {
        let path = PathBuf::from(&option.input);
        if !path.exists() {
            bail!("{} does not exist", option.input);
        }
        (option.input.clone(), path.with_extension(""))
    };

    let base = match &option.out {
        Some(out) if out.is_dir() => out.join(default_base.file_name().unwrap_or_default()),
        Some(out) => out.with_extension(""),
        None => default_base,
    };

    let began = Instant::now();
    eprintln!("splaude: decoding audio");
    let decoded = audio::decode(&source);
    let _ = std::fs::remove_dir_all(&scratch);
    let sample = Arc::new(decoded?);
    let duration = audio::duration_ms(&sample);

    eprintln!(
        "splaude: {} of audio · {backend} · {} at a time · {} bias term{}{}",
        clock(duration),
        parallel,
        keyterm.len(),
        if keyterm.len() == 1 { "" } else { "s" },
        if keyterm.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                keyterm
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        backend = if backend_name.is_empty() {
            "anthropic"
        } else {
            &backend_name
        },
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let started = Instant::now();
    let report = move |progress: Progress| {
        let fraction = progress.heard_ms as f64 / progress.total_ms.max(1) as f64;
        let elapsed = started.elapsed().as_secs_f64();
        let left = if fraction > 0.02 {
            elapsed / fraction - elapsed
        } else {
            f64::NAN
        };
        eprintln!(
            "splaude: {:>3.0}% · piece {}/{} done · {}",
            fraction * 100.0,
            progress.chunk_done,
            progress.chunk_total,
            if left.is_finite() {
                format!("about {} left", clock((left * 1000.0) as u64))
            } else {
                "estimating…".into()
            },
        );
    };
    let transcript = runtime.block_on(transcribe::run(
        Request {
            sample,
            parallel,
            backend,
        },
        report,
    ))?;

    if transcript.word.is_empty() {
        eprintln!("splaude: no speech was recognised — nothing written");
        return Ok(());
    }

    if let Some(parent) = base.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut written = Vec::new();
    for format in &option.format {
        let body = match format {
            Format::Srt => cue::srt(&transcript.cue),
            Format::Vtt => cue::vtt(&transcript.cue),
            Format::Txt => cue::txt(&transcript.cue),
            Format::Json => cue::json(
                &option.input,
                &setting.language,
                transcript.duration_ms,
                &transcript.cue,
                &transcript.word,
            ),
        };
        let path = PathBuf::from(format!("{}.{}", base.display(), format.extension()));
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }

    if option.stdout {
        print!("{}", cue::txt(&transcript.cue));
    }
    eprintln!(
        "splaude: {} words, {} cues from {} pieces in {}",
        transcript.word.len(),
        transcript.cue.len(),
        transcript.chunk_count,
        clock(began.elapsed().as_millis() as u64)
    );
    for path in written {
        println!("{}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn arg(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_srt_and_txt_four_at_a_time() {
        let option = parse(&arg(&["talk.mp4"])).unwrap();
        assert_eq!(option.input, "talk.mp4");
        assert_eq!(option.format, [Format::Srt, Format::Txt]);
        assert_eq!(option.parallel, 4);
    }

    #[test]
    fn every_flag_is_read() {
        let option = parse(&arg(&[
            "-f",
            "all",
            "-c",
            "about CRISPR",
            "-t",
            "Okonkwo",
            "-l",
            "es",
            "-p",
            "3",
            "x.mkv",
            "-o",
            "out/sub",
        ]))
        .unwrap();
        assert_eq!(option.format.len(), 4);
        assert_eq!(option.context, ["about CRISPR"]);
        assert_eq!(option.term, ["Okonkwo"]);
        assert_eq!(option.language.as_deref(), Some("es"));
        assert_eq!(option.parallel, 3);
        assert_eq!(option.out, Some(PathBuf::from("out/sub")));
    }

    #[test]
    fn nonsense_is_refused_with_a_reason() {
        assert!(parse(&arg(&[])).is_err());
        assert!(parse(&arg(&["a.mp4", "-p", "99"])).is_err());
        assert!(parse(&arg(&["a.mp4", "-f", "docx"])).is_err());
        assert!(parse(&arg(&["a.mp4", "b.mp4"])).is_err());
    }
}
