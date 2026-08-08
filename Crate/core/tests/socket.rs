//! Drives the speech backend's socket loop against a local WebSocket server.
//!
//! The unit tests in `anthropic.rs` cover the pure parts — frame handling,
//! keyterm packing, the URL contract. Everything that only happens once a
//! socket is open (the keepalive on connect, audio framing, CloseStream, the
//! close grace, committing text the server never endpointed, and what a 4xx on
//! the upgrade does to the cached credential) needs a server to talk to.
//!
//! No test here reaches Anthropic.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use splaude_core::credential::{Credential, CredentialError, CredentialSource, Store};
use splaude_core::setting::Setting;
use splaude_core::speech::{AnthropicSpeechBackend, Session, SpeechBackend, SpeechEvent};

/// Long enough to absorb scheduling noise, short enough that a hang fails the
/// run rather than parking it.
const PATIENCE: Duration = Duration::from_secs(10);

// MARK: - Harness

/// Serves exactly one WebSocket connection, then stops. Returns its port.
async fn serve<F, Fut>(handler: F) -> u16
where
    F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        handler(socket).await;
    });

    port
}

/// Answers the upgrade with a raw HTTP status instead of completing it.
async fn refuse(status: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.expect("accept");

        // Read the request before answering it. Closing a socket that still
        // has unread bytes in it sends an RST rather than a FIN on Windows,
        // and the client then reports a reset connection instead of the status
        // this test exists to check.
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(count) => seen.extend_from_slice(&chunk[..count]),
            }
        }

        let _ = stream
            .write_all(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    });

    port
}

fn credential() -> Credential {
    Credential {
        access_token: "sk-test".into(),
        expires_at: None,
        source: "test".into(),
    }
}

fn backend(port: u16) -> AnthropicSpeechBackend {
    AnthropicSpeechBackend::new(credential(), &Setting::default())
        .with_endpoint(format!("ws://127.0.0.1:{port}/voice"))
}

fn open(backend: AnthropicSpeechBackend) -> (Session, mpsc::UnboundedReceiver<SpeechEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let session = backend.start(event_tx).expect("start");
    (session, event_rx)
}

/// Collects events until the session closes.
async fn drain(mut event: mpsc::UnboundedReceiver<SpeechEvent>) -> Vec<SpeechEvent> {
    let collect = async {
        let mut seen = Vec::new();
        while let Some(what) = event.recv().await {
            let last = matches!(what, SpeechEvent::Close);
            seen.push(what);
            if last {
                break;
            }
        }
        seen
    };

    tokio::time::timeout(PATIENCE, collect)
        .await
        .expect("session never closed")
}

fn transcribe(text: &str, is_final: bool) -> SpeechEvent {
    SpeechEvent::Transcribe {
        text: text.into(),
        is_final,
    }
}

// MARK: - Tests

#[tokio::test]
async fn opens_transcribes_and_commits_on_an_endpoint_frame() {
    let port = serve(|mut socket| async move {
        socket
            .send(Message::text(
                r#"{"type":"TranscriptInterim","data":"low testing"}"#,
            ))
            .await
            .unwrap();
        socket
            .send(Message::text(
                r#"{"type":"TranscriptText","data":"one two three"}"#,
            ))
            .await
            .unwrap();
        socket
            .send(Message::text(r#"{"type":"TranscriptEndpoint"}"#))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    })
    .await;

    let (_session, event) = open(backend(port));
    let seen = drain(event).await;

    assert_eq!(seen.first(), Some(&SpeechEvent::Open));
    assert!(seen.contains(&transcribe("low testing", false)));
    assert!(seen.contains(&transcribe("one two three", false)));
    // The endpoint frame commits whatever was last provisional — not the first.
    assert!(seen.contains(&transcribe("one two three", true)));
    assert_eq!(seen.last(), Some(&SpeechEvent::Close));
}

#[tokio::test]
async fn sends_a_keepalive_as_soon_as_the_socket_opens() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = serve(move |mut socket| async move {
        if let Some(Ok(first)) = socket.next().await {
            let _ = seen_tx.send(first);
        }
        let _ = socket.close(None).await;
    })
    .await;

    let (_session, event) = open(backend(port));

    let first = tokio::time::timeout(PATIENCE, seen_rx.recv())
        .await
        .expect("nothing arrived")
        .expect("channel closed");
    assert_eq!(first, Message::text(r#"{"type":"KeepAlive"}"#));

    drain(event).await;
}

#[tokio::test]
async fn forwards_audio_as_binary_frames() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = serve(move |mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if message.is_binary() {
                let _ = seen_tx.send(message);
                break;
            }
        }
        let _ = socket.close(None).await;
    })
    .await;

    let (session, event) = open(backend(port));
    session.send_audio(vec![1, 2, 3, 4]);

    let audio = tokio::time::timeout(PATIENCE, seen_rx.recv())
        .await
        .expect("no audio arrived")
        .expect("channel closed");
    assert_eq!(audio, Message::binary(vec![1, 2, 3, 4]));

    drain(event).await;
}

#[tokio::test]
async fn finish_sends_closestream_then_ends_the_take() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = serve(move |mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = &message {
                if text.as_str().contains("CloseStream") {
                    let _ = seen_tx.send(text.to_string());
                    break;
                }
            }
        }
        // Deliberately do not close: the client's grace timer must end the
        // take on its own, or a finished dictation would hang on a server that
        // never hangs up.
        std::future::pending::<()>().await;
    })
    .await;

    let (session, event) = open(backend(port));
    session.finish();

    let closing = tokio::time::timeout(PATIENCE, seen_rx.recv())
        .await
        .expect("CloseStream never arrived")
        .expect("channel closed");
    assert!(closing.contains("CloseStream"));

    assert_eq!(drain(event).await.last(), Some(&SpeechEvent::Close));
}

// MARK: - Pacing the flush

/// Forwards every message the client sends, stamped with when it arrived, and
/// hangs up once CloseStream lands so the take does not sit out its grace.
async fn record(seen: mpsc::UnboundedSender<(Message, std::time::Instant)>) -> u16 {
    serve(move |mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            let is_closing =
                matches!(&message, Message::Text(text) if text.as_str().contains("CloseStream"));
            let _ = seen.send((message, std::time::Instant::now()));
            if is_closing {
                break;
            }
        }
        let _ = socket.close(None).await;
    })
    .await
}

/// One capture buffer: 32 ms at 16 kHz mono int16.
const CHUNK_BYTE: usize = 1_024;
const CHUNK_SPAN: Duration = Duration::from_millis(32);

/// The truncation bug in miniature: a take shorter than the handshake queues
/// whole, floods out the moment the socket opens, and is finished before the
/// server has heard a word of it.
///
/// Two things must hold. The audio must reach the server *before* CloseStream —
/// the channel is unbounded and ordered, so it does, and this pins that down.
/// And CloseStream must not follow it instantly, or the server finalises on
/// whatever fraction it has decoded.
#[tokio::test]
async fn a_burst_reaches_the_server_before_a_held_closestream() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = record(seen_tx).await;

    let (session, event) = open(backend(port));

    // Half a second of audio at 16 kHz mono int16, handed over all at once.
    let chunk_byte = 1_024;
    let chunk_count = 16;
    for index in 0..chunk_count {
        session.send_audio(vec![index as u8; chunk_byte]);
    }
    let started = std::time::Instant::now();
    session.finish();

    let mut audio_byte = 0_usize;
    let mut closing_at = None;
    while let Ok(Some((message, at))) = tokio::time::timeout(PATIENCE, seen_rx.recv()).await {
        match message {
            Message::Binary(data) => {
                assert!(
                    closing_at.is_none(),
                    "audio arrived after CloseStream — the take was cut short"
                );
                audio_byte += data.len();
            }
            Message::Text(text) if text.as_str().contains("CloseStream") => {
                closing_at = Some(at);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        audio_byte,
        chunk_byte * chunk_count,
        "the server did not get the whole take before the close"
    );

    let held = closing_at.expect("CloseStream never arrived") - started;
    // The audio ran ~512 ms ahead of the clock, and the close comes that much
    // later — most of it spent pacing the audio out, the remainder holding.
    // Slack below it for scheduling, none of consequence above.
    assert!(
        held >= Duration::from_millis(400),
        "CloseStream went out {held:?} after the take ended — the server had no \
         time to decode the burst"
    );

    drain(event).await;
}

/// The fix the hold alone could not deliver: a backlog must reach the server
/// *spread over time*, because a streaming recogniser decodes against stream
/// time. Handing it a block and then waiting buys it nothing; handing it the
/// same block at the rate it was spoken lets it decode while the clock runs.
#[tokio::test]
async fn a_burst_is_written_out_at_roughly_the_rate_it_was_recorded() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = record(seen_tx).await;

    let (session, event) = open(backend(port));

    // 1.6 s of audio, queued behind the handshake and handed over all at once.
    let chunk_count = 50;
    for index in 0..chunk_count {
        session.send_audio(vec![index as u8; CHUNK_BYTE]);
    }
    session.finish();

    let mut arrival = Vec::new();
    let mut closing_at = None;
    while let Ok(Some((message, at))) = tokio::time::timeout(PATIENCE, seen_rx.recv()).await {
        match message {
            Message::Binary(_) => arrival.push(at),
            Message::Text(text) if text.as_str().contains("CloseStream") => {
                closing_at = Some(at);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(arrival.len(), chunk_count, "not every chunk arrived");
    let first = arrival[0];
    let last = *arrival.last().expect("chunk arrived");

    // The head start is deliberate and bounded: the server may hold a quarter
    // second of not-yet-spoken audio, and no more. Anything past that is the
    // file-copy flush this test exists to forbid.
    let prompt_count = arrival
        .iter()
        .filter(|at| **at - first < Duration::from_millis(150))
        .count();
    assert!(
        prompt_count < chunk_count / 2,
        "{prompt_count} of {chunk_count} chunks arrived at once — the flush was not paced"
    );

    // Spread over roughly the take's own length, less the head start.
    let span = last - first;
    assert!(
        span >= Duration::from_millis(1_000),
        "the whole take was delivered in {span:?} — the server had no clock to decode against"
    );
    // And no longer than the take: pacing may not cost more than the hold it
    // replaced. Generous above for a coarse Windows timer, but bounded.
    assert!(
        span <= CHUNK_SPAN * chunk_count as u32 + Duration::from_millis(700),
        "the flush took {span:?}, longer than the audio it was pacing"
    );

    let closing_at = closing_at.expect("CloseStream never arrived");
    assert!(
        closing_at >= last,
        "CloseStream overtook the audio it was meant to follow"
    );

    drain(event).await;
}

/// Live typing must keep working through a paced flush. The read branch shares
/// the loop with the pacing timer, so an interim sent while audio is still
/// going out has to reach the client *then*, not once the flush is done.
#[tokio::test]
async fn interims_arrive_while_a_burst_is_still_being_paced_out() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = serve(move |mut socket| async move {
        let mut is_spoken = false;
        while let Some(Ok(message)) = socket.next().await {
            let is_closing =
                matches!(&message, Message::Text(text) if text.as_str().contains("CloseStream"));
            let _ = seen_tx.send((message.clone(), std::time::Instant::now()));
            if message.is_binary() && !is_spoken {
                is_spoken = true;
                let _ = socket
                    .send(Message::text(
                        r#"{"type":"TranscriptInterim","data":"mid flush"}"#,
                    ))
                    .await;
            }
            if is_closing {
                break;
            }
        }
        let _ = socket.close(None).await;
    })
    .await;

    let (session, mut event) = open(backend(port));

    for index in 0..50_u8 {
        session.send_audio(vec![index; CHUNK_BYTE]);
    }
    session.finish();

    // Stamp the interim the moment the client surfaces it.
    let interim_at = tokio::time::timeout(PATIENCE, async {
        while let Some(what) = event.recv().await {
            if what == transcribe("mid flush", false) {
                return std::time::Instant::now();
            }
        }
        panic!("the interim never reached the client");
    })
    .await
    .expect("no interim while the flush was running");

    let mut closing_at = None;
    while let Ok(Some((message, at))) = tokio::time::timeout(PATIENCE, seen_rx.recv()).await {
        if matches!(&message, Message::Text(text) if text.as_str().contains("CloseStream")) {
            closing_at = Some(at);
            break;
        }
    }

    assert!(
        interim_at < closing_at.expect("CloseStream never arrived"),
        "the interim only surfaced after the take ended — pacing blocked the read branch"
    );

    drain(event).await;
}

/// The invariant the whole design turns on: audio that never outran the clock
/// is never delayed by a millisecond. Every buffer must reach the server as
/// soon as it is handed over, exactly as it did before pacing existed.
#[tokio::test]
async fn audio_arriving_at_the_pace_it_was_spoken_is_written_straight_through() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = record(seen_tx).await;

    let (session, event) = open(backend(port));

    // Wait for the socket before timing anything: the connect is what a real
    // backlog forms behind, and this test is about the steady state after it.
    let mut sent = Vec::new();
    for index in 0..10_u8 {
        session.send_audio(vec![index; CHUNK_BYTE]);
        sent.push(std::time::Instant::now());
        tokio::time::sleep(CHUNK_SPAN).await;
    }
    session.finish();

    let mut arrival = Vec::new();
    while let Ok(Some((message, at))) = tokio::time::timeout(PATIENCE, seen_rx.recv()).await {
        match message {
            Message::Binary(_) => arrival.push(at),
            Message::Text(text) if text.as_str().contains("CloseStream") => break,
            _ => {}
        }
    }

    assert_eq!(arrival.len(), sent.len(), "not every chunk arrived");
    // Skip the first: it leaves once the socket is up, which is a connect
    // delay rather than a pacing one.
    for (index, (at, when)) in arrival.iter().zip(&sent).enumerate().skip(1) {
        let delay = at.saturating_duration_since(*when);
        assert!(
            // Any pacing sleep is at least a quarter second, so this cannot
            // pass with one hidden in it.
            delay < Duration::from_millis(150),
            "chunk {index} was delayed {delay:?} on a take that never fell behind"
        );
    }

    drain(event).await;
}

/// The other half of the same guarantee: a take whose audio kept pace with the
/// clock owes nothing, and must close exactly as it always did. This is the
/// long-take path — no latency may be added to it.
#[tokio::test]
async fn a_take_that_kept_pace_closes_without_waiting() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let port = record(seen_tx).await;

    let (session, event) = open(backend(port));

    // 32 ms of audio every 32 ms, which is what a live microphone looks like.
    for index in 0..6_u8 {
        session.send_audio(vec![index; 1_024]);
        tokio::time::sleep(Duration::from_millis(32)).await;
    }
    let started = std::time::Instant::now();
    session.finish();

    let mut closing_at = None;
    while let Ok(Some((message, at))) = tokio::time::timeout(PATIENCE, seen_rx.recv()).await {
        if matches!(&message, Message::Text(text) if text.as_str().contains("CloseStream")) {
            closing_at = Some(at);
            break;
        }
    }

    let held = closing_at.expect("CloseStream never arrived") - started;
    assert!(
        held < Duration::from_millis(250),
        "CloseStream was held {held:?} on a take that never outran real time"
    );

    drain(event).await;
}

#[tokio::test]
async fn dropping_the_session_ends_the_take() {
    let port = serve(|mut socket| async move { while socket.next().await.is_some() {} }).await;

    let (session, event) = open(backend(port));
    drop(session);

    assert_eq!(drain(event).await.last(), Some(&SpeechEvent::Close));
}

#[tokio::test]
async fn text_the_server_never_endpointed_is_still_committed() {
    let port = serve(|mut socket| async move {
        socket
            .send(Message::text(
                r#"{"type":"TranscriptInterim","data":"unfinished words"}"#,
            ))
            .await
            .unwrap();
        // Hang up mid-utterance, with no TranscriptEndpoint.
        socket.close(None).await.unwrap();
    })
    .await;

    let (_session, event) = open(backend(port));
    let seen = drain(event).await;

    // Losing these would silently drop the tail of a dictation.
    assert!(
        seen.contains(&transcribe("unfinished words", true)),
        "pending text was dropped: {seen:?}"
    );
}

#[tokio::test]
async fn a_transcript_error_frame_is_reported_without_being_fatal() {
    let port = serve(|mut socket| async move {
        socket
            .send(Message::text(
                r#"{"type":"TranscriptError","description":"audio too quiet"}"#,
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    })
    .await;

    let (_session, event) = open(backend(port));
    let seen = drain(event).await;

    assert!(seen.contains(&SpeechEvent::Fail {
        message: "audio too quiet".into(),
        fatal: false,
    }));
}

// MARK: - Credential rejection

/// Counts how often the store actually goes back to its source, which is the
/// only way to observe the cache from outside.
struct CountingSource {
    read_count: Arc<AtomicUsize>,
}

impl CredentialSource for CountingSource {
    fn name(&self) -> String {
        "counting".into()
    }

    fn read(&self) -> Result<Option<Vec<u8>>, CredentialError> {
        self.read_count.fetch_add(1, Ordering::SeqCst);
        Ok(Some(br#"{"accessToken":"sk-test"}"#.to_vec()))
    }
}

#[tokio::test]
async fn a_401_is_fatal_and_drops_the_cached_credential() {
    let port = refuse("401 Unauthorized").await;

    let read_count = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Store::new(vec![Box::new(CountingSource {
        read_count: read_count.clone(),
    })]));

    store.load().expect("first load");
    store.load().expect("second load");
    // Held for the session, so a second load must not go back to the source.
    assert_eq!(read_count.load(Ordering::SeqCst), 1);

    let (_session, event) = open(backend(port).with_store(store.clone()));
    let seen = drain(event).await;

    assert!(
        seen.iter().any(|what| matches!(
            what,
            SpeechEvent::Fail { fatal: true, message } if message.contains("401")
        )),
        "expected a fatal rejection, got {seen:?}"
    );

    // The cache must have been dropped, or every retry reuses the dead token.
    store.load().expect("reload");
    assert_eq!(
        read_count.load(Ordering::SeqCst),
        2,
        "the rejected credential was not evicted"
    );
}

#[tokio::test]
async fn a_500_is_reported_but_not_fatal() {
    let port = refuse("500 Internal Server Error").await;

    let (_session, event) = open(backend(port));
    let seen = drain(event).await;

    // A server fault is worth retrying; a rejected credential is not.
    assert!(
        seen.iter().any(|what| matches!(
            what,
            SpeechEvent::Fail { fatal: false, message } if message.contains("500")
        )),
        "expected a non-fatal server error, got {seen:?}"
    );
}

#[tokio::test]
async fn an_unreachable_server_fails_without_hanging() {
    // Nothing is listening here: bind a port, then drop the listener.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr").port()
    };

    let (_session, event) = open(backend(port));
    let seen = drain(event).await;

    assert!(
        seen.iter()
            .any(|what| matches!(what, SpeechEvent::Fail { .. })),
        "expected a connection failure, got {seen:?}"
    );
    assert_eq!(seen.last(), Some(&SpeechEvent::Close));
}
