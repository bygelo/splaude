//! Dictation through Deepgram directly, with a key of your own.
//!
//! This exists to make one sentence in the README true. splaude's speech goes
//! to an endpoint that is undocumented, internal to Anthropic, and not
//! sanctioned for this use; the README says so plainly, and offers as mitigation
//! that `SpeechBackend` is a small trait so another provider could be dropped
//! in. That was an argument about the shape of the code, and it had never been
//! tested — the trait had exactly one implementor, and the app named it
//! directly. An abstraction with one implementation is a claim.
//!
//! It is a claim worth cashing rather than merely fixing on paper. Anthropic
//! began rejecting third-party use of Claude Code OAuth tokens in early 2026 and
//! several tools built on that credential stopped working the same week, with no
//! notice. splaude's speech endpoint still answers today. The point of this file
//! is that the day it stops, the fix is a setting rather than a rewrite.
//!
//! Deepgram is the provider chosen because it is already what is on the other
//! end: the Anthropic endpoint runs Nova-3, so the same audio, the same
//! keyterms and the same interim/final rhythm apply, and a user switching over
//! should notice the bill and nothing else.
//!
//! # What is deliberately not here
//!
//! No pacing, no backlog accounting, no held close. The Anthropic backend
//! carries all three because that endpoint decodes at roughly real time over
//! *stream* time, so a short take flushed in one burst gets committed as a
//! fragment. Deepgram's streaming API does not behave that way — it finalises
//! on `CloseStream` — and copying machinery across on the assumption that two
//! servers share a quirk would be inventing a bug to be symmetrical.

use std::time::Duration;

use anyhow::anyhow;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::diagnostic;
use crate::speech::anthropic::pack_keyterm;
use crate::speech::{Frame, Session, SpeechAudioFormat, SpeechBackend, SpeechEvent};

/// Deepgram's streaming endpoint.
const ENDPOINT: &str = "wss://api.deepgram.com/v1/listen";

/// Deepgram closes an idle socket after 12 seconds. The Anthropic backend uses
/// 8 for the same reason and the same margin.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(8);

/// How long to wait for the final transcript after asking the stream to close.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

const KEEP_ALIVE_FRAME: &str = r#"{"type":"KeepAlive"}"#;
const CLOSE_FRAME: &str = r#"{"type":"CloseStream"}"#;

pub struct DeepgramSpeechBackend {
    key: String,
    language: String,
    keyterm: Vec<String>,
}

impl DeepgramSpeechBackend {
    /// Fails rather than connecting without a key, so the error names the
    /// missing setting instead of arriving later as a rejected handshake.
    pub fn new(setting: &crate::setting::Setting) -> anyhow::Result<Self> {
        let key = setting.api_key.trim().to_string();
        if key.is_empty() {
            return Err(anyhow!(
                "speechBackend is \"deepgram\" but apiKey is empty — put a Deepgram API key in {}",
                crate::setting::Setting::path().display()
            ));
        }

        Ok(Self {
            key,
            language: setting.language.clone(),
            keyterm: setting.keyterm(),
        })
    }

    fn url(&self) -> String {
        let format = SpeechAudioFormat::LINEAR16_16K;

        // `interim_results` is what live typing is built on: without it nothing
        // arrives until an utterance ends and the whole "appears as you speak"
        // property is gone. `smart_format` is what supplies the punctuation and
        // casing the Anthropic endpoint is asked for with `forward_interims`.
        let mut url = format!(
            "{ENDPOINT}?model=nova-3&encoding=linear16&sample_rate={}&channels={}\
             &interim_results=true&smart_format=true&punctuate=true&language={}",
            format.sample_rate,
            format.channel_count,
            urlencode(&self.language),
        );

        // Deepgram spells the same idea as a repeated query parameter rather
        // than a header, but the budget and the packing rules are the server's
        // and are shared, so the same packer decides what fits.
        let packed = pack_keyterm(&self.keyterm);
        for term in packed.split(',').filter(|term| !term.is_empty()) {
            url.push_str("&keyterm=");
            url.push_str(&urlencode(term));
        }

        url
    }
}

impl SpeechBackend for DeepgramSpeechBackend {
    fn audio_format(&self) -> SpeechAudioFormat {
        SpeechAudioFormat::LINEAR16_16K
    }

    fn start(&self, event: mpsc::UnboundedSender<SpeechEvent>) -> anyhow::Result<Session> {
        let mut request = self.url().into_client_request()?;
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Token {}", self.key))?,
        );

        let (frame_tx, frame_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            run(request, frame_rx, event).await;
        });

        Ok(Session::new(frame_tx))
    }
}

async fn run(
    request: http::Request<()>,
    mut frame: mpsc::UnboundedReceiver<Frame>,
    event: mpsc::UnboundedSender<SpeechEvent>,
) {
    crate::speech::anthropic::ensure_crypto_provider();

    let emit = |what: SpeechEvent| {
        let _ = event.send(what);
    };

    let socket = match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _response)) => socket,
        Err(error) => {
            // Named as Deepgram's, because a user who has just switched
            // providers needs to know which end refused them — a bad key here
            // fails identically to a dead endpoint there.
            emit(SpeechEvent::Fail {
                message: format!("could not open the Deepgram socket: {error}"),
                fatal: true,
            });
            emit(SpeechEvent::Close);
            return;
        }
    };

    diagnostic::log("stt", "deepgram: open");
    emit(SpeechEvent::Open);

    let (mut write, mut read) = socket.split();
    let mut keep_alive = tokio::time::interval(KEEP_ALIVE_INTERVAL);
    // The first tick is immediate, and a keepalive before any audio is a frame
    // the server has no reason to see.
    keep_alive.tick().await;

    let mut closing = false;
    let mut pending = String::new();

    loop {
        tokio::select! {
            outgoing = frame.recv(), if !closing => match outgoing {
                Some(Frame::Audio(pcm)) => {
                    if write.send(Message::Binary(pcm.into())).await.is_err() {
                        break;
                    }
                }
                // `CloseStream` rather than dropping the socket: it asks the
                // server to finalise what it has decoded, which is where the
                // last few words of every take come from.
                Some(Frame::Close) | None => {
                    let _ = write.send(Message::Text(CLOSE_FRAME.into())).await;
                    closing = true;
                }
            },

            _ = keep_alive.tick(), if !closing => {
                if write.send(Message::Text(KEEP_ALIVE_FRAME.into())).await.is_err() {
                    break;
                }
            }

            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if digest(&text, &mut pending, &emit) {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    emit(SpeechEvent::Fail {
                        message: format!("deepgram socket error: {error}"),
                        fatal: true,
                    });
                    break;
                }
                None => break,
            },

            // Only armed once the close has gone out, so an ordinary take is
            // never cut short by it.
            () = tokio::time::sleep(CLOSE_GRACE), if closing => {
                diagnostic::log("stt", "deepgram: close grace expired");
                break;
            }
        }
    }

    // Whatever the server never marked final is still what the user said, and
    // losing it because a socket closed early is the failure this take exists
    // to avoid.
    if !pending.is_empty() {
        emit(SpeechEvent::Transcribe {
            text: std::mem::take(&mut pending),
            is_final: true,
        });
    }

    diagnostic::log("stt", "deepgram: closing");
    emit(SpeechEvent::Close);
}

/// Reads one server frame. Answers whether the stream is over.
fn digest(text: &str, pending: &mut String, emit: &impl Fn(SpeechEvent)) -> bool {
    let Ok(frame) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };

    match frame.get("type").and_then(serde_json::Value::as_str) {
        Some("Results") => {
            let transcript = frame
                .get("channel")
                .and_then(|channel| channel.get("alternatives"))
                .and_then(|alternative| alternative.get(0))
                .and_then(|first| first.get("transcript"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();

            // Deepgram sends empty interim results while nobody is speaking.
            // Committing one would clear the pending utterance and lose it.
            if transcript.is_empty() {
                return false;
            }

            let is_final = frame
                .get("is_final")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if is_final {
                pending.clear();
            } else {
                pending.clear();
                pending.push_str(transcript);
            }

            emit(SpeechEvent::Transcribe {
                text: transcript.to_string(),
                is_final,
            });
            false
        }

        // Sent after `CloseStream` once the server has finalised.
        Some("Metadata") => true,

        Some("Error") | Some("error") => {
            emit(SpeechEvent::Fail {
                message: frame
                    .get("description")
                    .or_else(|| frame.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("deepgram error")
                    .to_string(),
                fatal: false,
            });
            false
        }

        _ => false,
    }
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::setting::Setting;

    fn backend(key: &str) -> DeepgramSpeechBackend {
        DeepgramSpeechBackend::new(&Setting {
            api_key: key.to_string(),
            ..Setting::default()
        })
        .unwrap()
    }

    #[test]
    fn a_missing_key_is_refused_before_any_socket_is_opened() {
        let setting = Setting {
            api_key: "   ".to_string(),
            ..Setting::default()
        };
        let refused = DeepgramSpeechBackend::new(&setting);
        assert!(refused.is_err());
        // The message has to name the setting, or the user is left guessing
        // which of two providers failed and why.
        let message = refused.err().unwrap().to_string();
        assert!(message.contains("apiKey"), "{message}");
    }

    #[test]
    fn the_url_carries_the_audio_contract_the_capture_stage_produces() {
        let url = backend("secret").url();
        assert!(url.starts_with(ENDPOINT));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("sample_rate=16000"));
        assert!(url.contains("channels=1"));
        // Live typing is built on interim results; without them the whole
        // "appears as you speak" property is gone.
        assert!(url.contains("interim_results=true"));
    }

    #[test]
    fn keyterms_ride_as_repeated_parameters() {
        let setting = Setting {
            api_key: "secret".into(),
            custom_keyterm: vec!["Supabase".into(), "splaude".into()],
            use_builtin_keyterm: false,
            ..Setting::default()
        };
        let url = DeepgramSpeechBackend::new(&setting).unwrap().url();
        assert!(url.contains("&keyterm=Supabase"));
        assert!(url.contains("&keyterm=splaude"));
    }

    #[test]
    fn a_language_with_a_region_is_escaped_rather_than_pasted() {
        let setting = Setting {
            api_key: "secret".into(),
            language: "en-GB".into(),
            ..Setting::default()
        };
        let url = DeepgramSpeechBackend::new(&setting).unwrap().url();
        assert!(url.contains("language=en-GB"));
    }

    /// The key is a secret and the URL is logged in error paths, so it must
    /// never be a query parameter — Deepgram takes it as a header.
    #[test]
    fn the_key_is_never_in_the_url() {
        let url = backend("sk-super-secret").url();
        assert!(!url.contains("sk-super-secret"));
        assert!(!url.contains("token"));
    }

    fn emitted(frames: &[&str]) -> Vec<SpeechEvent> {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let mut pending = String::new();
        {
            let emit = |what: SpeechEvent| seen.borrow_mut().push(what);
            for frame in frames {
                digest(frame, &mut pending, &emit);
            }
        }
        seen.into_inner()
    }

    #[test]
    fn an_interim_result_is_provisional_and_a_final_commits() {
        let seen = emitted(&[
            r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":"hello"}]}}"#,
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"hello there"}]}}"#,
        ]);

        assert_eq!(
            seen,
            vec![
                SpeechEvent::Transcribe {
                    text: "hello".into(),
                    is_final: false
                },
                SpeechEvent::Transcribe {
                    text: "hello there".into(),
                    is_final: true
                },
            ]
        );
    }

    /// Deepgram sends empty interims through silence. Emitting them would clear
    /// the display between words.
    #[test]
    fn an_empty_transcript_is_not_an_utterance() {
        let seen = emitted(&[
            r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":""}]}}"#,
        ]);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_frame_that_is_not_json_is_ignored_rather_than_fatal() {
        assert!(emitted(&["not json at all", "{}", r#"{"type":"Unknown"}"#]).is_empty());
    }

    #[test]
    fn an_error_frame_is_reported_without_ending_the_take() {
        let seen = emitted(&[r#"{"type":"Error","description":"rate limited"}"#]);
        assert_eq!(
            seen,
            vec![SpeechEvent::Fail {
                message: "rate limited".into(),
                fatal: false
            }]
        );
    }

    #[test]
    fn metadata_ends_the_stream() {
        let mut pending = String::new();
        let emit = |_: SpeechEvent| {};
        assert!(digest(r#"{"type":"Metadata"}"#, &mut pending, &emit));
        assert!(!digest(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"x"}]}}"#,
            &mut pending,
            &emit
        ));
    }

    /// The unfinalised tail is what the user said last, and a socket that ends
    /// without a final frame must not swallow it.
    #[test]
    fn an_interim_is_left_pending_so_the_tail_survives_a_close() {
        let mut pending = String::new();
        let emit = |_: SpeechEvent| {};
        digest(
            r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":"half a sentence"}]}}"#,
            &mut pending,
            &emit,
        );
        assert_eq!(pending, "half a sentence");

        // And a final clears it, so it is not inserted twice.
        digest(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"half a sentence"}]}}"#,
            &mut pending,
            &emit,
        );
        assert!(pending.is_empty());
    }
}
