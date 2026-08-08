//! Streams microphone audio to the WebSocket the Claude Code IDE extension uses
//! for its dictation button. The endpoint, query parameter and framing below are
//! taken verbatim from the shipped extension bundle:
//!
//! ```text
//! wss://api.anthropic.com/api/ws/speech_to_text/voice_stream
//!   ?encoding=linear16&sample_rate=16000&channels=1
//!   &endpointing_ms=300&utterance_end_ms=1000&language=en
//!   &use_conversation_engine=true&stt_provider=deepgram-nova3
//! ```
//!
//! It is an undocumented internal endpoint authenticated with the Claude Code
//! OAuth token. It can change or disappear without notice.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::credential::{Credential, Store};
use crate::diagnostic;
use crate::quota;
use crate::speech::{Frame, Session, SpeechAudioFormat, SpeechBackend, SpeechEvent};

// MARK: - Wire constants (from the extension bundle)

const ENDPOINT: &str = "wss://api.anthropic.com/api/ws/speech_to_text/voice_stream";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(8);
const CLOSE_GRACE: Duration = Duration::from_secs(3);
const KEYTERM_BYTE_BUDGET: usize = 1024;

/// How far ahead of the clock the writer is allowed to run — the amount of
/// not-yet-spoken audio the server may be holding at any moment.
///
/// Zero would be the purest reading of "pace it like a microphone", but it
/// costs a timer wakeup per 32 ms buffer, and a Windows timer that rounds up by
/// its own granularity would stretch the flush well past the take. A quarter
/// second is coarse enough that a 2 s backlog costs a handful of sleeps, and
/// short enough to stay inside `endpointing_ms=300` — the server never carries
/// enough unheard audio to change where it decides an utterance ended.
const PACE_LEAD: Duration = Duration::from_millis(250);

/// Ceiling on the total delay pacing and the close hold may add to one take.
/// Not a tuning knob — a stop. The delay is bounded by the take's own length
/// already, so reaching this means the socket took most of a dictation to come
/// up, and a user staring at a finished take is served worse by waiting longer
/// than by an imperfect transcript.
const LAG_CAP: Duration = Duration::from_secs(10);

const KEEP_ALIVE_FRAME: &str = r#"{"type":"KeepAlive"}"#;
const CLOSE_FRAME: &str = r#"{"type":"CloseStream"}"#;

/// rustls 0.23 refuses to pick a crypto provider on its own once more than one
/// could exist, and a missing provider surfaces as a confusing handshake panic
/// rather than a connection error. Install ours once, on first connect.
fn ensure_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Errs only if something already installed one, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub struct AnthropicSpeechBackend {
    credential: Credential,
    keyterm: Vec<String>,
    language: String,
    /// Asks the server to shape interim results for typing — punctuated and
    /// cased as they stream, rather than raw words to be cleaned up at the end.
    typed_interim: bool,
    /// Held so a rejection can drop the cached token. See the 401 handling
    /// below for why that matters.
    store: Option<Arc<Store>>,
    endpoint: String,
}

impl AnthropicSpeechBackend {
    pub fn new(credential: Credential, setting: &crate::setting::Setting) -> Self {
        Self {
            credential,
            keyterm: setting.keyterm(),
            language: setting.language.clone(),
            typed_interim: setting.live_typing,
            store: None,
            endpoint: ENDPOINT.to_string(),
        }
    }

    /// Lets a rejected credential be evicted from the cache that produced it.
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Points the session at a different server, leaving the wire contract
    /// alone. Exists so the socket loop can be driven against a local server in
    /// tests without reaching Anthropic.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    fn url(&self) -> String {
        let format = SpeechAudioFormat::LINEAR16_16K;
        let mut url = format!(
            "{}?encoding=linear16&sample_rate={}&channels={}\
             &endpointing_ms=300&utterance_end_ms=1000&language={}\
             &use_conversation_engine=true&stt_provider=deepgram-nova3",
            self.endpoint,
            format.sample_rate,
            format.channel_count,
            urlencode(&self.language),
        );
        if self.typed_interim {
            url.push_str("&forward_interims=typed");
        }
        url
    }
}

impl SpeechBackend for AnthropicSpeechBackend {
    fn audio_format(&self) -> SpeechAudioFormat {
        SpeechAudioFormat::LINEAR16_16K
    }

    fn start(&self, event: mpsc::UnboundedSender<SpeechEvent>) -> anyhow::Result<Session> {
        // Build the handshake first, then decorate it. Constructing the request
        // by hand instead skips Sec-WebSocket-Key, Upgrade and Connection, and
        // the server rejects the upgrade — which looks exactly like a network
        // failure from the outside.
        let mut request = self.url().into_client_request()?;
        let header = request.headers_mut();

        header.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {}", self.credential.access_token))?,
        );
        header.insert("x-app", http::HeaderValue::from_static("vscode"));

        let packed = pack_keyterm(&self.keyterm);
        if !packed.is_empty() {
            header.insert("x-config-keyterms", http::HeaderValue::from_str(&packed)?);
        }

        let (frame_tx, frame_rx) = mpsc::unbounded_channel();
        let store = self.store.clone();

        tokio::spawn(async move {
            run(request, frame_rx, event, store).await;
        });

        Ok(Session::new(frame_tx))
    }
}

// MARK: - Backlog

/// How far ahead of real time the audio written to the socket has run.
///
/// The recogniser on the other end is a *streaming* one — `endpointing_ms` and
/// `utterance_end_ms` are windows over a live stream, and `CloseStream` makes it
/// finalise on whatever it has decoded so far. The handshake to
/// `api.anthropic.com` takes a second or more, and audio captured before the
/// socket is up queues locally, so it would leave in one burst the instant the
/// connection opens. On a long take that burst is followed by seconds of live
/// speech and nobody notices. On a take shorter than the handshake the whole
/// dictation goes out in milliseconds with `CloseStream` right behind it, and
/// the server commits a fragment of a sentence.
///
/// Holding `CloseStream` for the length of the burst was the first attempt at
/// that, and it did not work: the server appears to decode at roughly real time
/// *over stream time*, so seconds of audio handed over in one write buy it no
/// time to decode them, and idle wall clock afterwards is not a substitute for
/// audio that arrives while it is listening. So the debt this tracks is now
/// spent where it does work — throttling the writer, so the flush looks like a
/// microphone instead of a file copy — and only the remainder is spent holding
/// the close.
///
/// Debt is the audio that arrived faster than it could have been spoken and has
/// not yet been paid back in wall clock. It drains with the clock, and audio
/// arriving no faster than real time adds nothing to it: a live feed asks the
/// server for no time it is not already being given. That asymmetry is the whole
/// point — it is what leaves a take that never fell behind untouched by any of
/// this, paced not at all and held not at all.
#[derive(Debug)]
struct Backlog {
    format: SpeechAudioFormat,
    /// When the last chunk went out, so the clock since then can pay the debt
    /// down. `None` until the first write, when there is nothing to measure
    /// against and nothing owed.
    last_write: Option<Instant>,
    owed: Duration,
}

impl Backlog {
    fn new(format: SpeechAudioFormat) -> Self {
        Self {
            format,
            last_write: None,
            owed: Duration::ZERO,
        }
    }

    /// Records a chunk of `byte_count` bytes handed to the socket at `at`.
    fn wrote(&mut self, byte_count: usize, at: Instant) {
        let spoken = self.format.duration_of(byte_count);
        let gap = self
            .last_write
            .map(|last| at.saturating_duration_since(last))
            .unwrap_or_default();

        // Wall clock since the previous chunk pays the debt down…
        self.owed = self.owed.saturating_sub(gap);
        // …and only the part of this chunk that outran the clock adds to it.
        self.owed += spoken.saturating_sub(gap);
        self.last_write = Some(at);
    }

    /// What the server is still owed as of `at`.
    fn owed(&self, at: Instant) -> Duration {
        match self.last_write {
            Some(last) => self.owed.saturating_sub(at.saturating_duration_since(last)),
            None => Duration::ZERO,
        }
    }
}

/// Owns the socket for the life of one take.
async fn run(
    request: http::Request<()>,
    mut frame: mpsc::UnboundedReceiver<Frame>,
    event: mpsc::UnboundedSender<SpeechEvent>,
    store: Option<Arc<Store>>,
) {
    ensure_crypto_provider();

    let emit = |what: SpeechEvent| {
        let _ = event.send(what);
    };

    let (socket, response) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(error) => {
            report_connect_failure(error, &store, &emit);
            emit(SpeechEvent::Close);
            return;
        }
    };

    // The 101 response carries whatever metering headers the endpoint uses;
    // this is where the "does it spend quota" question gets answered.
    quota::mark_connected();
    quota::record(
        response.status().as_u16(),
        response
            .headers()
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value))),
    );

    diagnostic::log("socket", "open");
    emit(SpeechEvent::Open);

    let (mut write, mut read) = socket.split();
    let _ = write.send(Message::text(KEEP_ALIVE_FRAME)).await;

    let mut keep_alive = tokio::time::interval(KEEP_ALIVE_INTERVAL);
    keep_alive.tick().await; // the first tick is immediate

    let grace = tokio::time::sleep(CLOSE_GRACE);
    tokio::pin!(grace);

    // Armed when the writer has run `PACE_LEAD` ahead of the clock, and again
    // when the take ends owing the remainder. Both are timers rather than
    // in-line sleeps so the read branch keeps running: interims arrive and live
    // typing carries on right through a paced flush.
    let pace = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(pace);
    let hold = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(hold);

    // Set once CloseStream is sent. The server then drops the connection, and
    // the still-running receive loop sees that as an error — an expected one,
    // which must not be reported or it buries the real status message.
    let mut is_finishing = false;
    // Set when the take ends. No further frame can arrive after it, so the
    // frame branch must stand down or a closed channel spins the loop.
    let mut is_ended = false;
    // Each guards its timer against being polled once already elapsed.
    let mut is_pacing = false;
    let mut is_holding = false;

    // Audio waiting for the clock to let it out. The capture side is unbounded
    // and never blocks, so a slow flush backs up here rather than in the mixer.
    let mut outbound: VecDeque<Vec<u8>> = VecDeque::new();
    let mut backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
    let mut lag = Duration::ZERO;
    let mut pending = String::new();

    loop {
        // Meter the queue out at roughly the rate it was recorded. Whatever the
        // clock already covers goes immediately — which for a live microphone is
        // every buffer, the instant it arrives, exactly as before pacing
        // existed. Only audio that has outrun the clock waits, and it waits here
        // rather than at the close, so the server is decoding during the delay
        // instead of being handed a block and a stopwatch.
        while !is_pacing && !outbound.is_empty() {
            let owed = backlog.owed(Instant::now());
            if owed >= PACE_LEAD && lag < LAG_CAP {
                let wait = owed.min(LAG_CAP - lag);
                if lag.is_zero() {
                    diagnostic::log(
                        "socket",
                        format!(
                            "audio outran real time by {} ms — pacing the flush",
                            owed.as_millis()
                        ),
                    );
                }
                lag += wait;
                pace.as_mut().reset(tokio::time::Instant::now() + wait);
                is_pacing = true;
                break;
            }

            let pcm = outbound.pop_front().expect("queue is not empty");
            let byte_count = pcm.len();
            if let Err(error) = write.send(Message::binary(pcm)).await {
                emit(SpeechEvent::Fail {
                    message: format!("audio send failed: {error}"),
                    fatal: false,
                });
            } else {
                backlog.wrote(byte_count, Instant::now());
            }
        }

        // The close is the last thing in the same queue. CloseStream finalises
        // the utterance, so sending it while the server is still a beat behind
        // commits a fragment; pacing leaves at most `PACE_LEAD` of that beat
        // outstanding, and this hands it back. A take that kept pace owes
        // nothing and closes as it always did — the timer fires on the next turn
        // of the loop.
        if is_ended && outbound.is_empty() && !is_pacing && !is_holding && !is_finishing {
            let owed = backlog
                .owed(Instant::now())
                .min(LAG_CAP.saturating_sub(lag));
            if !owed.is_zero() {
                diagnostic::log(
                    "socket",
                    format!("holding CloseStream {} ms", owed.as_millis()),
                );
            }
            hold.as_mut().reset(tokio::time::Instant::now() + owed);
            is_holding = true;
        }

        tokio::select! {
            incoming = frame.recv(), if !is_ended => match incoming {
                Some(Frame::Audio(pcm)) => outbound.push_back(pcm),
                // An explicit finish and a dropped Session mean the same thing.
                Some(Frame::Close) | None => is_ended = true,
            },

            message = read.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    handle(text.as_bytes(), &mut pending, &emit);
                }
                Some(Ok(Message::Binary(data))) => {
                    handle(&data, &mut pending, &emit);
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    if is_finishing {
                        diagnostic::log("socket", "closed after CloseStream (expected)");
                    } else {
                        emit(SpeechEvent::Fail {
                            message: format!("WebSocket error: {error}"),
                            fatal: false,
                        });
                    }
                    break;
                }
                None => break,
            },

            _ = &mut pace, if is_pacing => is_pacing = false,

            _ = &mut hold, if is_holding => {
                is_holding = false;
                is_finishing = true;
                diagnostic::log("socket", "closing");
                let _ = write.send(Message::text(CLOSE_FRAME)).await;
                // Give the server a moment to flush a trailing endpoint event.
                grace.as_mut().reset(tokio::time::Instant::now() + CLOSE_GRACE);
            }

            _ = keep_alive.tick(), if !is_finishing => {
                let _ = write.send(Message::text(KEEP_ALIVE_FRAME)).await;
            }

            _ = &mut grace, if is_finishing => break,
        }
    }

    // Whatever the server had not yet endpointed is still the user's words.
    if !pending.is_empty() {
        emit(SpeechEvent::Transcribe {
            text: std::mem::take(&mut pending),
            is_final: true,
        });
    }

    let _ = write.close().await;
    emit(SpeechEvent::Close);
}

fn handle(data: &[u8], pending: &mut String, emit: &impl Fn(SpeechEvent)) {
    let Ok(frame) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(kind) = frame.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };

    match kind {
        // Both interim and text frames are provisional; the endpoint frame is
        // what actually commits an utterance. This matches the extension.
        "TranscriptInterim" | "TranscriptText" => {
            let Some(text) = frame.get("data").and_then(serde_json::Value::as_str) else {
                return;
            };
            if text.is_empty() {
                return;
            }
            pending.clear();
            pending.push_str(text);
            emit(SpeechEvent::Transcribe {
                text: text.to_string(),
                is_final: false,
            });
        }

        "TranscriptEndpoint" => {
            diagnostic::log("stt", format!("endpoint — commit \"{pending}\""));
            if !pending.is_empty() {
                emit(SpeechEvent::Transcribe {
                    text: std::mem::take(pending),
                    is_final: true,
                });
            }
        }

        "TranscriptError" => emit(SpeechEvent::Fail {
            message: frame
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("transcription error")
                .to_string(),
            fatal: false,
        }),

        "error" => emit(SpeechEvent::Fail {
            message: frame
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("server error")
                .to_string(),
            fatal: false,
        }),

        _ => {}
    }
}

/// A 4xx on the upgrade means the credential was rejected — that is fatal and
/// worth surfacing differently from a dropped connection.
fn report_connect_failure(error: WsError, store: &Option<Arc<Store>>, emit: &impl Fn(SpeechEvent)) {
    if let WsError::Http(response) = &error {
        let status = response.status().as_u16();
        if status >= 400 {
            let fatal = (400..500).contains(&status);
            let hint = if fatal {
                format!("credential rejected (HTTP {status}) — run `claude` to re-authenticate")
            } else {
                format!("server error (HTTP {status})")
            };

            // The token is held for the session to avoid a secret-store prompt
            // per take, so a rejection is the only signal that the cached copy
            // went stale ahead of its stated expiry. Drop it or every retry
            // reuses the same dead token.
            if status == 401 || status == 403 {
                if let Some(store) = store {
                    store.invalidate();
                }
            }

            emit(SpeechEvent::Fail {
                message: hint,
                fatal,
            });
            return;
        }
    }

    emit(SpeechEvent::Fail {
        message: format!("connection failed: {error}"),
        fatal: false,
    });
}

/// Comma-joined, deduped, ASCII-only, truncated to the server's budget — the
/// same normalisation the extension applies before sending keyterms.
pub fn pack_keyterm(term: &[String]) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<String> = Vec::new();
    let mut length = 0usize;

    for raw in term {
        // Commas are the separator, so they cannot survive inside a term.
        let clean: String = raw
            .replace(',', " ")
            .chars()
            .filter(|character| (' '..='~').contains(character))
            .collect();
        let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");

        if clean.is_empty() || seen.contains(&clean) {
            continue;
        }

        let cost = clean.chars().count() + usize::from(!kept.is_empty());
        if length + cost > KEYTERM_BYTE_BUDGET {
            break;
        }

        seen.insert(clean.clone());
        kept.push(clean);
        length += cost;
    }

    kept.join(",")
}

/// Only the handful of characters a BCP-47 tag could plausibly carry.
fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => character.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    fn term(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn joins_term_with_commas() {
        assert_eq!(pack_keyterm(&term(&["grep", "regex"])), "grep,regex");
    }

    #[test]
    fn drops_a_duplicate() {
        assert_eq!(
            pack_keyterm(&term(&["grep", "grep", "regex"])),
            "grep,regex"
        );
    }

    #[test]
    fn strips_a_comma_inside_a_term() {
        // A comma would otherwise split one term into two on the wire.
        assert_eq!(pack_keyterm(&term(&["one,two"])), "one two");
    }

    #[test]
    fn strips_non_ascii_and_collapses_whitespace() {
        assert_eq!(pack_keyterm(&term(&["  héllo   wörld  "])), "hllo wrld");
    }

    #[test]
    fn skips_a_term_that_is_empty_after_cleaning() {
        assert_eq!(pack_keyterm(&term(&["   ", "日本語", "kept"])), "kept");
    }

    #[test]
    fn stops_at_the_byte_budget_rather_than_truncating_a_term() {
        let long = "x".repeat(600);
        let packed = pack_keyterm(&term(&[&long, &long.replace('x', "y"), "zzz"]));
        // Two 600-char terms plus a separator exceed 1024, so the second is
        // dropped whole — and the loop stops there rather than backfilling.
        assert_eq!(packed, long);
        assert!(packed.len() <= KEYTERM_BYTE_BUDGET);
    }

    #[test]
    fn an_empty_list_packs_to_nothing() {
        assert_eq!(pack_keyterm(&[]), "");
    }

    #[test]
    fn builtin_keyterm_fits_inside_the_budget() {
        // If the shipped list ever outgrows the budget, terms would silently
        // vanish from the wire.
        let packed = pack_keyterm(&crate::setting::Setting::default().keyterm());
        assert!(packed.len() <= KEYTERM_BYTE_BUDGET);
        assert!(packed.contains("IntelliSense"));
        assert!(packed.contains("worktree"));
    }

    #[test]
    fn url_carries_the_wire_contract() {
        let backend = AnthropicSpeechBackend::new(
            Credential {
                access_token: "sk".into(),
                expires_at: None,
                source: "test".into(),
            },
            &crate::setting::Setting::default(),
        );
        let url = backend.url();
        assert!(url.starts_with(ENDPOINT));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("sample_rate=16000"));
        assert!(url.contains("channels=1"));
        assert!(url.contains("endpointing_ms=300"));
        assert!(url.contains("utterance_end_ms=1000"));
        assert!(url.contains("use_conversation_engine=true"));
        assert!(url.contains("stt_provider=deepgram-nova3"));
        assert!(url.contains("language=en"));
        // live_typing defaults on, which is what asks for typed interims.
        assert!(url.contains("forward_interims=typed"));
    }

    #[test]
    fn interims_are_not_requested_when_live_typing_is_off() {
        let setting = crate::setting::Setting {
            live_typing: false,
            ..Default::default()
        };
        let backend = AnthropicSpeechBackend::new(
            Credential {
                access_token: "sk".into(),
                expires_at: None,
                source: "test".into(),
            },
            &setting,
        );
        assert!(!backend.url().contains("forward_interims"));
    }

    #[test]
    fn language_tag_is_escaped_into_the_query() {
        let setting = crate::setting::Setting {
            language: "en-GB".into(),
            ..Default::default()
        };
        let backend = AnthropicSpeechBackend::new(
            Credential {
                access_token: "sk".into(),
                expires_at: None,
                source: "test".into(),
            },
            &setting,
        );
        assert!(backend.url().contains("language=en-GB"));
    }

    // MARK: - Backlog

    /// One capture buffer: 32 ms at 16 kHz mono int16.
    const CHUNK_BYTE: usize = 1_024;
    const CHUNK_SPAN: Duration = Duration::from_millis(32);

    fn close_to(left: Duration, right: Duration) -> bool {
        left.max(right) - left.min(right) < Duration::from_millis(5)
    }

    #[test]
    fn audio_arriving_at_the_pace_it_was_spoken_owes_nothing() {
        let mut backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
        let base = Instant::now();
        for index in 0..50_u32 {
            backlog.wrote(CHUNK_BYTE, base + CHUNK_SPAN * index);
        }
        // The first chunk is the only one that ever outran the clock, and the
        // second chunk's gap paid it back.
        assert_eq!(backlog.owed(base + CHUNK_SPAN * 50), Duration::ZERO);
    }

    #[test]
    fn a_burst_owes_the_time_it_saved() {
        let mut backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
        let base = Instant::now();
        // A whole 3.2 s take, queued during the handshake and flushed in 16 ms.
        for index in 0..100_u32 {
            backlog.wrote(CHUNK_BYTE, base + Duration::from_micros(160) * index);
        }
        // Owed: the whole take, less the handful of milliseconds the flush
        // itself took. Nothing else paid any of it back.
        let owed = backlog.owed(base + Duration::from_millis(16));
        assert!(
            owed > Duration::from_millis(3_100) && owed <= Duration::from_millis(3_200),
            "expected roughly the take's own length, got {owed:?}"
        );
    }

    #[test]
    fn what_is_owed_drains_with_the_clock() {
        let mut backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
        let base = Instant::now();
        backlog.wrote(CHUNK_BYTE * 10, base);
        assert!(close_to(backlog.owed(base), Duration::from_millis(320)));
        assert!(close_to(
            backlog.owed(base + Duration::from_millis(200)),
            Duration::from_millis(120)
        ));
        assert_eq!(backlog.owed(base + Duration::from_secs(1)), Duration::ZERO);
    }

    /// The regression that matters for the normal case: a long take opens with
    /// the same burst a short one does, but the speech that follows it arrives
    /// live, and by the time the user stops talking the debt is long gone.
    #[test]
    fn a_long_take_owes_nothing_by_the_time_the_speaker_stops() {
        let mut backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
        let base = Instant::now();

        // 1.3 s of audio queued behind the handshake, flushed in 10 ms.
        let burst_count = 1_300 / 32;
        for index in 0..burst_count {
            backlog.wrote(CHUNK_BYTE, base + Duration::from_micros(250) * index);
        }
        assert!(backlog.owed(base + Duration::from_millis(10)) > Duration::from_secs(1));

        // Then twenty more seconds of speech, arriving as it is spoken.
        let mut at = base + Duration::from_millis(10);
        for _ in 0..(20_000 / 32) {
            at += CHUNK_SPAN;
            backlog.wrote(CHUNK_BYTE, at);
        }
        assert_eq!(backlog.owed(at), Duration::ZERO);
    }

    #[test]
    fn a_take_with_no_audio_at_all_owes_nothing() {
        let backlog = Backlog::new(SpeechAudioFormat::LINEAR16_16K);
        assert_eq!(backlog.owed(Instant::now()), Duration::ZERO);
    }

    #[test]
    fn an_interim_frame_becomes_a_provisional_event() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut pending = String::new();
        handle(
            br#"{"type":"TranscriptInterim","data":"hello"}"#,
            &mut pending,
            &|what| seen.lock().unwrap().push(what),
        );
        assert_eq!(pending, "hello");
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![SpeechEvent::Transcribe {
                text: "hello".into(),
                is_final: false
            }]
        );
    }

    #[test]
    fn an_endpoint_frame_commits_what_was_pending() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut pending = "hello".to_string();
        handle(br#"{"type":"TranscriptEndpoint"}"#, &mut pending, &|what| {
            seen.lock().unwrap().push(what)
        });
        assert!(pending.is_empty());
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![SpeechEvent::Transcribe {
                text: "hello".into(),
                is_final: true
            }]
        );
    }

    #[test]
    fn an_endpoint_frame_with_nothing_pending_commits_nothing() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut pending = String::new();
        handle(br#"{"type":"TranscriptEndpoint"}"#, &mut pending, &|what| {
            seen.lock().unwrap().push(what)
        });
        assert!(seen.into_inner().unwrap().is_empty());
    }

    #[test]
    fn an_error_frame_reports_the_server_message() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut pending = String::new();
        handle(
            br#"{"type":"error","message":"bad audio"}"#,
            &mut pending,
            &|what| seen.lock().unwrap().push(what),
        );
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![SpeechEvent::Fail {
                message: "bad audio".into(),
                fatal: false
            }]
        );
    }

    #[test]
    fn unknown_and_malformed_frames_are_ignored() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut pending = String::new();
        let record = |what| seen.lock().unwrap().push(what);
        handle(br#"{"type":"SomethingNew"}"#, &mut pending, &record);
        handle(br#"not json"#, &mut pending, &record);
        handle(br#"{"no":"type"}"#, &mut pending, &record);
        assert!(seen.into_inner().unwrap().is_empty());
    }
}
