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
