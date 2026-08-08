//! Records what the speech connection's handshake says about rate limiting.
//!
//! Anthropic's Claude-metered endpoints answer with `anthropic-ratelimit-*`
//! headers describing remaining requests and tokens. If the speech socket
//! returns none of them, nothing on the Claude meter was touched — which is the
//! closest thing to proof available from the client side, short of watching the
//! account's usage page across a long dictation.

use std::sync::{Mutex, OnceLock};

use crate::diagnostic;

/// Header names that would indicate this request counted against something.
const INTERESTING: [&str; 6] = [
    "anthropic-ratelimit",
    "x-ratelimit",
    "ratelimit",
    "anthropic-organization",
    "retry-after",
    "x-should-retry",
];

#[derive(Default)]
struct State {
    last_header: Vec<(String, String)>,
    saw_rate_limit_header: bool,
    ever_connected: bool,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Notes that a handshake completed at all, so the summary can distinguish
/// "asked and saw nothing" from "never asked".
pub fn mark_connected() {
    if let Ok(mut held) = state().lock() {
        held.ever_connected = true;
    }
}

/// What a handshake's headers amount to, with no global state involved.
#[derive(Debug, Default, PartialEq)]
pub struct Capture {
    /// Interesting headers only, lowercased and sorted.
    pub header: Vec<(String, String)>,
    pub saw_rate_limit: bool,
    /// Every header name seen, for the log.
    pub seen: Vec<String>,
}

impl Capture {
    /// One-line rendering of this capture alone.
    pub fn summary(&self) -> String {
        if !self.saw_rate_limit {
            return "none seen".into();
        }
        let mut line: Vec<String> = self
            .header
            .iter()
            .filter(|(name, _)| name.contains("ratelimit"))
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        line.sort();
        line.join(", ")
    }
}

/// Pure header triage. Split from [`record`] so the filtering rules can be
/// tested without touching the process-wide state that the UI reads.
pub fn capture<'a>(header: impl IntoIterator<Item = (&'a str, &'a str)>) -> Capture {
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for (name, value) in header {
        let lower = name.to_ascii_lowercase();
        seen.push(lower.clone());
        if INTERESTING.iter().any(|prefix| lower.starts_with(prefix)) {
            kept.push((lower, value.to_string()));
        }
    }

    kept.sort();
    seen.sort();

    Capture {
        saw_rate_limit: kept.iter().any(|(name, _)| name.contains("ratelimit")),
        header: kept,
        seen,
    }
}

/// Records the upgrade response's headers.
///
/// Takes name/value pairs rather than any particular HTTP type so this stays
/// independent of whichever client the platform ends up using.
pub fn record<'a>(status: u16, header: impl IntoIterator<Item = (&'a str, &'a str)>) {
    let Capture {
        header: captured,
        saw_rate_limit,
        seen,
    } = capture(header);

    diagnostic::log("quota", format!("handshake HTTP {status}"));

    if captured.is_empty() {
        diagnostic::log(
            "quota",
            "no rate-limit headers — nothing metered on this connection",
        );
    } else {
        for (name, value) in &captured {
            diagnostic::log("quota", format!("{name}: {value}"));
        }
    }

    // Anything unexpected is worth seeing in full, once, rather than being
    // silently filtered out by the list above.
    diagnostic::log("quota", format!("all headers: {}", seen.join(", ")));

    if let Ok(mut held) = state().lock() {
        held.last_header = captured;
        held.saw_rate_limit_header = saw_rate_limit;
    }
}

/// What this run has learned about metering, as a value rather than a sentence.
///
/// The three real answers are genuinely different claims and flattening them
/// would be a lie in one direction or the other: "we asked and the endpoint
/// reported no rate limit" is the evidence the README's central claim rests on,
/// and "we have not asked yet" is not evidence of anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// No handshake has completed this run, so nothing has been observed.
    Unknown,
    /// A handshake completed and answered with no rate-limit header at all.
    Unmetered,
    /// The endpoint reported a rate limit. Carries the readings, joined.
    Metered(String),
    /// The shared state could not be read.
    Unavailable,
}

impl Reading {
    /// One line for a menu item or the `--check` report.
    pub fn line(&self) -> String {
        match self {
            Reading::Unknown => "dictate once to check".into(),
            Reading::Unmetered => "nothing metered (no rate-limit header)".into(),
            Reading::Metered(what) => what.clone(),
            Reading::Unavailable => "unavailable".into(),
        }
    }
}

/// What the last handshake said.
pub fn reading() -> Reading {
    let Ok(held) = state().lock() else {
        return Reading::Unavailable;
    };

    if !held.saw_rate_limit_header {
        return if held.ever_connected {
            Reading::Unmetered
        } else {
            Reading::Unknown
        };
    }

    let mut line: Vec<String> = held
        .last_header
        .iter()
        .filter(|(name, _)| name.contains("ratelimit"))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    line.sort();
    Reading::Metered(line.join(", "))
}

/// One-line answer for the interface.
pub fn summary() -> String {
    reading().line()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn keeps_only_the_interesting_header() {
        let capture = capture([
            ("Server", "nginx"),
            ("Anthropic-RateLimit-Requests-Remaining", "42"),
            ("Content-Type", "text/plain"),
        ]);
        assert!(capture.saw_rate_limit);
        assert_eq!(capture.header.len(), 1);
        assert_eq!(
            capture.summary(),
            "anthropic-ratelimit-requests-remaining=42"
        );
    }

    #[test]
    fn a_handshake_with_no_meter_header_reads_as_none_seen() {
        let capture = capture([("Server", "nginx"), ("Upgrade", "websocket")]);
        assert!(!capture.saw_rate_limit);
        assert!(capture.header.is_empty());
        assert_eq!(capture.summary(), "none seen");
        // The point of the whole module: this is the evidence nothing metered.
        assert_eq!(capture.seen, vec!["server", "upgrade"]);
    }

    /// The distinction the whole type exists for. Never having asked is not the
    /// same claim as having asked and seen nothing, and the interface has to say
    /// which one it means — the second is evidence, the first is silence.
    #[test]
    fn never_asked_and_asked_and_saw_nothing_read_differently() {
        assert_ne!(Reading::Unknown.line(), Reading::Unmetered.line());
        assert_eq!(Reading::Unknown.line(), "dictate once to check");
        assert!(
            Reading::Unmetered.line().contains("nothing metered"),
            "{}",
            Reading::Unmetered.line()
        );
    }

    #[test]
    fn a_metered_reading_shows_the_header_it_read() {
        let reading = Reading::Metered("anthropic-ratelimit-requests-remaining=42".into());
        assert_eq!(reading.line(), "anthropic-ratelimit-requests-remaining=42");
    }

    #[test]
    fn every_reading_says_something() {
        // A blank menu item reads as a broken app, so no variant may render
        // empty — including the one nobody expects to see.
        for reading in [
            Reading::Unknown,
            Reading::Unmetered,
            Reading::Metered("x=1".into()),
            Reading::Unavailable,
        ] {
            assert!(!reading.line().is_empty(), "{reading:?}");
        }
    }

    #[test]
    fn header_matching_is_case_insensitive_and_prefix_based() {
        let capture = capture([("RETRY-AFTER", "30"), ("x-ratelimit-limit", "9")]);
        assert_eq!(capture.header.len(), 2);
        // retry-after is interesting but is not itself a rate-limit reading.
        assert!(capture.saw_rate_limit);
    }
}
