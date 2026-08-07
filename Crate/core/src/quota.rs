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

/// One-line answer for the settings window.
pub fn summary() -> String {
    let Ok(held) = state().lock() else {
        return "unavailable".into();
    };

    if held.last_header.is_empty() && !held.saw_rate_limit_header {
        return if held.ever_connected {
            "none seen".into()
        } else {
            "dictate once to check".into()
        };
    }

    if !held.saw_rate_limit_header {
        return "none seen".into();
    }

    let mut line: Vec<String> = held
        .last_header
        .iter()
        .filter(|(name, _)| name.contains("ratelimit"))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    line.sort();
    line.join(", ")
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

    #[test]
    fn header_matching_is_case_insensitive_and_prefix_based() {
        let capture = capture([("RETRY-AFTER", "30"), ("x-ratelimit-limit", "9")]);
        assert_eq!(capture.header.len(), 2);
        // retry-after is interesting but is not itself a rate-limit reading.
        assert!(capture.saw_rate_limit);
    }
}
