//! Transcribing a file — a video, a recording, anything ffmpeg can read — into
//! timed words, subtitle cues and a readable transcript.
//!
//! The dictation path streams a microphone; this path streams a file through
//! the very same [`SpeechBackend`], paced as if it were a microphone, because
//! that pacing is what makes the arrival of each word a usable timestamp (see
//! [`word`]). To keep an hour of video from taking an hour, the audio is cut
//! at its pauses ([`split`]) and the pieces run on parallel sockets.

pub mod align;
pub mod audio;
pub mod context;
pub mod cue;
pub mod split;
pub mod word;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;

use crate::speech::{SpeechBackend, SpeechEvent};
pub use cue::Cue;
pub use split::Chunk;
pub use word::{Envelope, Word};

/// One socket write. A microphone delivers buffers about this size.
const FRAME_MS: u64 = 100;
/// Silence fed after each piece so its last word is decoded before the close.
const TAIL_MS: u64 = 1_200;
/// After the tail, silence keeps flowing until the transcript has not changed
/// for this long. `CloseStream` finalises only what the server has decoded: a
/// session that fell 12 s behind lost its last two sentences to a fixed tail.
const QUIET_MS: u64 = 2_000;
/// Most silence fed after a piece while waiting for a lagging server.
const TAIL_MAX_MS: u64 = 30_000;
/// A socket that has not opened by now is not going to.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// Tries per piece before the whole run gives up.
const ATTEMPT: usize = 3;
/// Gap between socket opens, so a burst of handshakes does not look like one.
const STAGGER: Duration = Duration::from_millis(250);
/// Waits for a free session before a piece gives up.
const CROWDED_RETRY: usize = 40;
/// The most sessions the Anthropic endpoint lets one account hold. See
/// [`is_crowded`].
pub const SESSION_LIMIT: usize = 5;

pub type BackendFactory = Arc<dyn Fn() -> Result<Box<dyn SpeechBackend>> + Send + Sync>;

pub struct Request {
    /// 16 kHz mono samples of the whole recording.
    pub sample: Arc<Vec<i16>>,
    /// Sockets open at once.
    pub parallel: usize,
    pub backend: BackendFactory,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// Audio streamed so far across every piece.
    pub heard_ms: u64,
    pub total_ms: u64,
    pub chunk_done: usize,
    pub chunk_total: usize,
}

pub struct Transcript {
    pub word: Vec<Word>,
    pub cue: Vec<Cue>,
    pub duration_ms: u64,
    pub chunk_count: usize,
}

#[derive(Debug)]
enum ChunkError {
    /// Retrying will not help — a rejected credential, a missing key.
    Fatal(String),
    Retry(String),
    /// The account already has as many sessions open as it is allowed.
    Crowded(String),
}

/// The endpoint allows five live sessions per account, measured: a sixth and
/// seventh opened together are answered `{"type":"TranscriptError",
/// "error_code":"rate_limit","limit":"concurrent","description":"Too many
/// active sessions. Try again shortly."}` and closed with 4029.
fn is_crowded(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("too many active sessions") || lower.contains("rate limit")
}

/// Streams one piece at 1× and returns its words, chunk-relative.
async fn transcribe_chunk(
    backend: &dyn SpeechBackend,
    sample: &[i16],
    heard: &AtomicU64,
) -> Result<Vec<Word>, ChunkError> {
    let (event_tx, mut event) = mpsc::unbounded_channel();
    let session = backend
        .start(event_tx)
        .map_err(|error| ChunkError::Fatal(format!("{error:#}")))?;

    // The clock starts at Open, not before: audio queued during the handshake
    // would leave in a burst and every timing in the piece would be late.
    match tokio::time::timeout(OPEN_TIMEOUT, event.recv()).await {
        Ok(Some(SpeechEvent::Open)) => {}
        Ok(Some(SpeechEvent::Fail {
            message,
            fatal: true,
        })) => return Err(ChunkError::Fatal(message)),
        Ok(Some(SpeechEvent::Fail { message, .. })) if is_crowded(&message) => {
            return Err(ChunkError::Crowded(message))
        }
        Ok(Some(SpeechEvent::Fail { message, .. })) => return Err(ChunkError::Retry(message)),
        Ok(_) => return Err(ChunkError::Retry("socket closed before opening".into())),
        Err(_) => return Err(ChunkError::Retry("socket did not open in time".into())),
    }

    let sent_ms = Arc::new(AtomicU64::new(0));
    // `sent_ms` when the transcript last changed.
    let changed_ms = Arc::new(AtomicU64::new(0));
    let duration = audio::duration_ms(sample);
    let feeder = {
        let sent_ms = sent_ms.clone();
        let changed_ms = changed_ms.clone();
        let session = session.clone();
        let byte = audio::to_byte(sample);
        let frame_byte = audio::sample_at(FRAME_MS) * 2;
        let silence = vec![0u8; frame_byte];
        tokio::spawn(async move {
            let start = Instant::now();
            let mut index = 0u64;
            let send = |frame: Vec<u8>, index: &mut u64| {
                session.send_audio(frame);
                *index += 1;
                sent_ms.store(*index * FRAME_MS, Ordering::Relaxed);
            };
            for frame in byte.chunks(frame_byte) {
                tokio::time::sleep_until(start + Duration::from_millis(index * FRAME_MS)).await;
                send(frame.to_vec(), &mut index);
            }
            let audio_ms = index * FRAME_MS;
            loop {
                let sent = index * FRAME_MS;
                let is_quiet = sent.saturating_sub(changed_ms.load(Ordering::Relaxed)) >= QUIET_MS;
                if sent >= audio_ms + TAIL_MAX_MS || (sent >= audio_ms + TAIL_MS && is_quiet) {
                    break;
                }
                tokio::time::sleep_until(start + Duration::from_millis(index * FRAME_MS)).await;
                send(silence.clone(), &mut index);
            }
            session.finish();
        })
    };

    let mut tracker = word::Tracker::new();
    let mut failure: Option<String> = None;
    let mut last_heard = 0;

    while let Some(item) = event.recv().await {
        let now = sent_ms.load(Ordering::Relaxed).min(duration);
        heard.fetch_add(now.saturating_sub(last_heard), Ordering::Relaxed);
        last_heard = last_heard.max(now);
        match item {
            SpeechEvent::Transcribe { text, is_final } => {
                changed_ms.store(sent_ms.load(Ordering::Relaxed), Ordering::Relaxed);
                tracker.apply(&text, is_final, now)
            }
            SpeechEvent::Fail {
                message,
                fatal: true,
            } => {
                feeder.abort();
                return Err(ChunkError::Fatal(message));
            }
            SpeechEvent::Fail { message, .. } if is_crowded(&message) => {
                feeder.abort();
                return Err(ChunkError::Crowded(message));
            }
            SpeechEvent::Fail { message, .. } => failure = Some(message),
            SpeechEvent::Close => break,
            SpeechEvent::Open => {}
        }
    }

    let finished_feeding = feeder.is_finished();
    feeder.abort();
    if let Some(message) = failure {
        return Err(ChunkError::Retry(message));
    }
    if !finished_feeding {
        return Err(ChunkError::Retry(
            "socket closed before the piece was sent".into(),
        ));
    }
    heard.fetch_add(duration.saturating_sub(last_heard), Ordering::Relaxed);

    let mut word = tracker.finish();
    // Benchmark switch: the tracker's raw estimates, for calibrating
    // `ARRIVAL_LATENCY_MS` without the snapping hiding its error.
    if std::env::var_os("SPLAUDE_RAW_TIMING").is_some() {
        for item in &mut word {
            item.start_ms = item.end_ms.min(duration);
            item.end_ms = item.start_ms;
        }
    } else {
        let envelope = Envelope::of(sample);
        dump_raw(&word, sample.len());
        let outcome = align::time(&mut word, &envelope);
        crate::diagnostic::log(
            "transcribe",
            if outcome.is_noise {
                format!(
                    "{} phrases for {} words: too noisy to align, arrival timing kept",
                    outcome.phrase_count,
                    word.len()
                )
            } else {
                format!(
                    "{} phrases: {} corrected for server lag, {} re-timed whole",
                    outcome.phrase_count, outcome.lag_corrected, outcome.retimed
                )
            },
        );
    }
    Ok(word)
}

/// Benchmark switch: `SPLAUDE_RAW_DUMP=<dir>` writes each piece's raw arrival
/// estimates, keyed by its length, so a run whose server lagged can be
/// replayed offline through `refine` and `align` (see `align::replay`).
fn dump_raw(word: &[Word], sample_count: usize) {
    let Some(dir) = std::env::var_os("SPLAUDE_RAW_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("raw-{sample_count}.json"));
    let body = serde_json::json!({ "word": word }).to_string();
    if let Err(error) = std::fs::write(&path, body) {
        crate::diagnostic::log(
            "transcribe",
            format!("raw dump to {} failed: {error}", path.display()),
        );
    }
}

/// Transcribes the whole recording. `report` is called every few seconds.
pub async fn run(
    request: Request,
    report: impl Fn(Progress) + Send + Sync + 'static,
) -> Result<Transcript> {
    let sample = request.sample;
    let duration = audio::duration_ms(&sample);
    let chunk = split::plan(&Envelope::of(&sample), request.parallel);
    crate::diagnostic::log(
        "transcribe",
        format!(
            "pieces: {}",
            chunk
                .iter()
                .map(|c| format!("{}–{}", c.start_ms, c.end_ms))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    let chunk_total = chunk.len();

    let heard = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicU64::new(0));
    let reporter = {
        let heard = heard.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                report(Progress {
                    heard_ms: heard.load(Ordering::Relaxed).min(duration),
                    total_ms: duration,
                    chunk_done: done.load(Ordering::Relaxed) as usize,
                    chunk_total,
                });
            }
        })
    };

    let gate = Arc::new(Semaphore::new(request.parallel.max(1)));
    // How many sockets the account is currently believed to allow. Shrinks
    // when the server says there are too many, never below one.
    let capacity = Arc::new(AtomicU64::new(request.parallel.max(1) as u64));
    let mut task = tokio::task::JoinSet::new();
    for (index, piece) in chunk.iter().copied().enumerate() {
        let gate = gate.clone();
        let capacity = capacity.clone();
        let sample = sample.clone();
        let heard = heard.clone();
        let done = done.clone();
        let factory = request.backend.clone();
        task.spawn(async move {
            if index < request.parallel {
                tokio::time::sleep(STAGGER * index as u32).await;
            }
            let slice = &sample[audio::sample_at(piece.start_ms)
                ..audio::sample_at(piece.end_ms).min(sample.len())];
            let mut last_error = String::new();
            let mut attempt = 0;
            let mut crowded = 0;
            while attempt < ATTEMPT {
                let permit = gate
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                let backend = factory().map_err(|error| anyhow!("{error:#}"))?;
                let before = heard.load(Ordering::Relaxed);
                let outcome = transcribe_chunk(backend.as_ref(), slice, &heard).await;
                // Un-count what a failed attempt claimed to have heard.
                if outcome.is_err() {
                    let after = heard.load(Ordering::Relaxed);
                    heard.fetch_sub(after.saturating_sub(before).min(after), Ordering::Relaxed);
                }
                match outcome {
                    Ok(mut word) => {
                        drop(permit);
                        // A finished piece is evidence the crowding has
                        // passed; win back one slot given up earlier. A wrong
                        // guess costs one rejected socket and a short wait.
                        if capacity
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                                (held < request.parallel as u64).then(|| held + 1)
                            })
                            .is_ok()
                        {
                            gate.add_permits(1);
                        }
                        for item in &mut word {
                            item.start_ms += piece.start_ms;
                            item.end_ms += piece.start_ms;
                        }
                        done.fetch_add(1, Ordering::Relaxed);
                        return Ok((index, word));
                    }
                    Err(ChunkError::Fatal(message)) => return Err(anyhow!("{message}")),
                    Err(ChunkError::Crowded(message)) => {
                        // Not this piece's fault: something else holds a
                        // session — splaude dictating, another client, or more
                        // sockets than the account allows. Give one slot up for
                        // good and wait for a free one; this does not count as
                        // an attempt, up to a point.
                        crowded += 1;
                        if crowded > CROWDED_RETRY {
                            return Err(anyhow!(
                                "{message} (still crowded after {CROWDED_RETRY} waits)"
                            ));
                        }
                        let shrunk = capacity
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                                (held > 1).then(|| held - 1)
                            })
                            .is_ok();
                        if shrunk {
                            permit.forget();
                            crate::diagnostic::log(
                                "transcribe",
                                format!(
                                    "server allows fewer sessions — down to {} at a time",
                                    capacity.load(Ordering::Relaxed)
                                ),
                            );
                        } else {
                            drop(permit);
                        }
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    Err(ChunkError::Retry(message)) => {
                        drop(permit);
                        attempt += 1;
                        crate::diagnostic::log(
                            "transcribe",
                            format!("piece {} attempt {attempt} failed: {message}", index + 1),
                        );
                        last_error = message;
                        tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                    }
                }
            }
            Err(anyhow!(
                "piece {} ({}–{} s) failed {ATTEMPT} times: {last_error}",
                index + 1,
                piece.start_ms / 1000,
                piece.end_ms / 1000
            ))
        });
    }

    let mut by_chunk: Vec<Vec<Word>> = vec![Vec::new(); chunk_total];
    while let Some(joined) = task.join_next().await {
        match joined.map_err(|error| anyhow!("a transcription task panicked: {error}"))? {
            Ok((index, word)) => by_chunk[index] = word,
            Err(error) => {
                task.abort_all();
                reporter.abort();
                return Err(error);
            }
        }
    }
    reporter.abort();

    let word: Vec<Word> = by_chunk.into_iter().flatten().collect();
    let cue = cue::cue_of(&word);
    Ok(Transcript {
        word,
        cue,
        duration_ms: duration,
        chunk_count: chunk_total,
    })
}
