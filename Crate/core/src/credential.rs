//! Reads the Claude Code OAuth credential that the CLI / IDE extension already
//! keeps on this machine.
//!
//! Three storage shapes exist in the wild:
//!
//!   1. macOS Keychain, generic password, service "Claude Code-credentials",
//!      whose *password* is the same JSON blob as (2).
//!   2. `~/.claude/.credentials.json` — the shape used on Windows and Linux,
//!      and the macOS fallback.
//!   3. Linux secret service, when Claude Code was able to reach one.
//!
//! All wrap the token as `{"claudeAiOauth": {"accessToken": …, "expiresAt": …}}`.
//! Older builds wrote the fields at the top level, so both layouts are accepted.
//!
//! Only (2) is portable, so it lives here. Anything backed by an OS secret store
//! is supplied by the platform crate through [`CredentialSource`] — which is
//! also what keeps the "try the secret store, then the file" ordering explicit
//! rather than buried in a `cfg`.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// How long to sit on an expired copy before going back to the source.
///
/// The cache is skipped once a token is past expiry so a refresh gets picked
/// up — but without a floor here, an expired credential that nobody is
/// refreshing means a secret-store hit, and possibly an authorization prompt,
/// on every status check.
const STALE_RECHECK: Duration = Duration::from_secs(10);

/// How close to expiry counts as worth warning about.
const WARN_WINDOW_MS: f64 = 10.0 * 60.0 * 1000.0;

/// The service name Claude Code files its credential under.
pub const SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub access_token: String,
    /// Unix epoch milliseconds. `None` when the blob omits it.
    pub expires_at: Option<f64>,
    /// Where it came from, for the diagnostic command.
    pub source: String,
}

impl Credential {
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(now_ms())
    }

    fn is_expired_at(&self, now: f64) -> bool {
        match self.expires_at {
            Some(expiry) => now >= expiry,
            None => false,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum CredentialError {
    #[error(
        "No Claude Code credential found. Run `claude` in a terminal and sign in, then try again."
    )]
    NotFound,
    #[error("Claude Code credential could not be parsed ({0}).")]
    Unreadable(String),
    #[error("Claude Code credential has expired. Run `claude` in a terminal to refresh it.")]
    Expired,
}

/// Somewhere a credential blob might live.
///
/// Implementations return `Ok(None)` for "looked, wasn't there" and `Err` only
/// for "looked, and something was wrong" — the difference decides whether the
/// next source gets a turn or the failure is worth reporting.
pub trait CredentialSource: Send + Sync {
    /// Shown to the user in the diagnostic output, so name the place.
    fn name(&self) -> String;
    fn read(&self) -> Result<Option<Vec<u8>>, CredentialError>;
}

/// `~/.claude/.credentials.json`.
pub struct FileSource {
    path: std::path::PathBuf,
}

impl Default for FileSource {
    fn default() -> Self {
        Self {
            path: dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join(".credentials.json"),
        }
    }
}

impl FileSource {
    pub fn at(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl CredentialSource for FileSource {
    fn name(&self) -> String {
        self.path.display().to_string()
    }

    fn read(&self) -> Result<Option<Vec<u8>>, CredentialError> {
        match std::fs::read(&self.path) {
            Ok(data) => Ok(Some(data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CredentialError::Unreadable(error.to_string())),
        }
    }
}

/// The credential's state, classified for display.
///
/// splaude reads this token but never refreshes it — that is Claude Code's job,
/// and it only happens while Claude Code runs. Someone who installs a dictation
/// app and never thinks about it as a Claude Code accessory can therefore find
/// it dead, so the state is worth surfacing before a take fails rather than at
/// the moment the hotkey is pressed.
#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    /// Unix epoch milliseconds, when stated.
    Usable {
        until: Option<f64>,
    },
    ExpiringSoon(f64),
    Expired,
    Missing(String),
}

impl Health {
    pub fn needs_attention(&self) -> bool {
        !matches!(self, Health::Usable { .. })
    }

    /// One line for the tray menu. `None` when there is nothing to say.
    pub fn headline(&self) -> Option<String> {
        match self {
            Health::Usable { .. } => None,
            Health::ExpiringSoon(at) => Some(format!(
                "Credential expires {} — run `claude` to refresh",
                short_time(*at)
            )),
            Health::Expired => Some("Credential expired — run `claude` in a terminal".into()),
            Health::Missing(_) => {
                Some("No Claude Code credential — run `claude` and sign in".into())
            }
        }
    }
}

#[derive(Default)]
struct Cache {
    credential: Option<Credential>,
    last_read: Option<SystemTime>,
}

/// Resolves a usable token from an ordered list of sources.
///
/// Cached across calls. On macOS every read of the Keychain item is an ACL
/// decision and the OS prompts for the login password unless the user clicked
/// *Always Allow*, so reading per take meant one prompt per dictation. Holding
/// it for the session fixes that; the cache is dropped once the token is past
/// its stated expiry so a refreshed credential is still picked up, at the cost
/// of exactly one prompt at that point rather than one per take.
pub struct Store {
    source: Vec<Box<dyn CredentialSource>>,
    cache: Mutex<Cache>,
}

impl Store {
    /// Sources are tried in order; the first that yields a parsable blob wins.
    pub fn new(source: Vec<Box<dyn CredentialSource>>) -> Self {
        Self {
            source,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// The file source alone — correct as-is on Windows and Linux.
    pub fn file_only() -> Self {
        Self::new(vec![Box::new(FileSource::default())])
    }

    pub fn load(&self) -> Result<Credential, CredentialError> {
        let mut cache = self.cache.lock().expect("credential cache poisoned");

        if let Some(held) = &cache.credential {
            if !held.is_expired() {
                return Ok(held.clone());
            }
            // Expired, but re-reading on every call would hammer the source.
            if let Some(last) = cache.last_read {
                if last.elapsed().unwrap_or(STALE_RECHECK) < STALE_RECHECK {
                    return Ok(held.clone());
                }
            }
        }

        cache.last_read = Some(SystemTime::now());
        let mut first_failure: Option<CredentialError> = None;

        for source in &self.source {
            match source.read() {
                Ok(Some(data)) => match parse(&data, &source.name()) {
                    Ok(credential) => {
                        cache.credential = Some(credential.clone());
                        return Ok(credential);
                    }
                    Err(error) => first_failure.get_or_insert(error),
                },
                Ok(None) => continue,
                Err(error) => first_failure.get_or_insert(error),
            };
        }

        Err(first_failure.unwrap_or(CredentialError::NotFound))
    }

    pub fn health(&self) -> Health {
        match self.load() {
            Ok(credential) => classify(&credential, now_ms()),
            Err(error) => Health::Missing(error.to_string()),
        }
    }

    /// Drops the cached copy so the next `load()` goes back to the source.
    /// Call when the server rejects the token — an expiry we were not told
    /// about looks exactly like a valid cached credential from here.
    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.credential = None;
        }
    }

    /// Reports what was found without ever revealing the secret. Used by
    /// `--check`.
    pub fn describe(&self) -> String {
        match self.load() {
            Ok(credential) => {
                let fingerprint: String = credential.access_token.chars().take(12).collect();
                let mut line = format!(
                    "found: {}\n  token: {}… ({} chars)",
                    credential.source,
                    fingerprint,
                    credential.access_token.chars().count()
                );
                match credential.expires_at {
                    Some(at) => line.push_str(&format!(
                        "\n  expires: {} — {}",
                        full_time(at),
                        if credential.is_expired() {
                            "EXPIRED"
                        } else {
                            "valid"
                        }
                    )),
                    None => line.push_str("\n  expires: not stated"),
                }
                line
            }
            Err(error) => format!("not found: {error}"),
        }
    }
}

/// Split out so the thresholds can be exercised against crafted credentials
/// rather than only whatever the machine happens to hold.
pub fn classify(credential: &Credential, now: f64) -> Health {
    let Some(expiry) = credential.expires_at else {
        return Health::Usable { until: None };
    };

    if expiry <= now {
        return Health::Expired;
    }
    if expiry - now <= WARN_WINDOW_MS {
        return Health::ExpiringSoon(expiry);
    }
    Health::Usable {
        until: Some(expiry),
    }
}

pub fn parse(data: &[u8], source: &str) -> Result<Credential, CredentialError> {
    let root: serde_json::Value = serde_json::from_slice(data)
        .map_err(|_| CredentialError::Unreadable(format!("not JSON — from {source}")))?;

    // Current layout nests under claudeAiOauth; older ones are flat.
    let scope = root.get("claudeAiOauth").unwrap_or(&root);

    let token = scope
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CredentialError::Unreadable(format!("no accessToken — from {source}")))?;

    Ok(Credential {
        access_token: token.to_string(),
        expires_at: scope.get("expiresAt").and_then(serde_json::Value::as_f64),
        source: source.to_string(),
    })
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn short_time(epoch_ms: f64) -> String {
    local_of(epoch_ms)
        .map(|at| at.format("%-I:%M %p").to_string())
        .unwrap_or_else(|| "soon".into())
}

fn full_time(epoch_ms: f64) -> String {
    local_of(epoch_ms)
        .map(|at| at.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn local_of(epoch_ms: f64) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(epoch_ms as i64).single()
}

#[cfg(test)]
mod test {
    use super::*;

    const NOW: f64 = 1_800_000_000_000.0;
    const MINUTE: f64 = 60_000.0;

    #[test]
    fn parses_the_nested_layout() {
        let blob = br#"{"claudeAiOauth":{"accessToken":"sk-abc","expiresAt":123}}"#;
        let credential = parse(blob, "test").unwrap();
        assert_eq!(credential.access_token, "sk-abc");
        assert_eq!(credential.expires_at, Some(123.0));
    }

    #[test]
    fn parses_the_older_flat_layout() {
        let blob = br#"{"accessToken":"sk-flat","expiresAt":456}"#;
        let credential = parse(blob, "test").unwrap();
        assert_eq!(credential.access_token, "sk-flat");
        assert_eq!(credential.expires_at, Some(456.0));
    }

    #[test]
    fn accepts_a_credential_with_no_stated_expiry() {
        let credential = parse(br#"{"accessToken":"sk-x"}"#, "test").unwrap();
        assert_eq!(credential.expires_at, None);
        assert!(!credential.is_expired());
        assert_eq!(classify(&credential, NOW), Health::Usable { until: None });
    }

    #[test]
    fn rejects_a_blob_with_no_token() {
        assert!(matches!(
            parse(br#"{"claudeAiOauth":{}}"#, "test"),
            Err(CredentialError::Unreadable(_))
        ));
    }

    #[test]
    fn rejects_an_empty_token() {
        assert!(matches!(
            parse(br#"{"accessToken":""}"#, "test"),
            Err(CredentialError::Unreadable(_))
        ));
    }

    #[test]
    fn rejects_text_that_is_not_json() {
        assert!(matches!(
            parse(b"not json at all", "test"),
            Err(CredentialError::Unreadable(_))
        ));
    }

    #[test]
    fn classifies_across_the_warn_window() {
        let at = |expiry: f64| Credential {
            access_token: "sk".into(),
            expires_at: Some(expiry),
            source: "test".into(),
        };

        assert_eq!(classify(&at(NOW - MINUTE), NOW), Health::Expired);
        assert_eq!(classify(&at(NOW), NOW), Health::Expired);
        assert_eq!(
            classify(&at(NOW + 5.0 * MINUTE), NOW),
            Health::ExpiringSoon(NOW + 5.0 * MINUTE)
        );
        assert_eq!(
            classify(&at(NOW + 60.0 * MINUTE), NOW),
            Health::Usable {
                until: Some(NOW + 60.0 * MINUTE)
            }
        );
    }

    #[test]
    fn only_a_usable_credential_needs_no_attention() {
        assert!(!Health::Usable { until: None }.needs_attention());
        assert!(Health::Expired.needs_attention());
        assert!(Health::Missing("gone".into()).needs_attention());
        assert!(Health::ExpiringSoon(NOW).needs_attention());
        assert!(Health::Usable { until: None }.headline().is_none());
        assert!(Health::Expired.headline().is_some());
    }

    struct Absent;
    impl CredentialSource for Absent {
        fn name(&self) -> String {
            "absent".into()
        }
        fn read(&self) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(None)
        }
    }

    struct Holding(&'static str);
    impl CredentialSource for Holding {
        fn name(&self) -> String {
            "holding".into()
        }
        fn read(&self) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(Some(self.0.as_bytes().to_vec()))
        }
    }

    #[test]
    fn falls_through_an_absent_source_to_the_next() {
        let store = Store::new(vec![
            Box::new(Absent),
            Box::new(Holding(r#"{"accessToken":"sk-second"}"#)),
        ]);
        assert_eq!(store.load().unwrap().access_token, "sk-second");
    }

    #[test]
    fn reports_not_found_when_every_source_is_empty() {
        let store = Store::new(vec![Box::new(Absent)]);
        assert_eq!(store.load(), Err(CredentialError::NotFound));
    }

    #[test]
    fn describe_never_prints_the_whole_token() {
        let secret = "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz";
        let store = Store::new(vec![Box::new(Holding(
            r#"{"accessToken":"sk-ant-oat01-abcdefghijklmnopqrstuvwxyz"}"#,
        ))]);
        let described = store.describe();
        assert!(!described.contains(secret));
        assert!(described.contains("sk-ant-oat01"));
    }

    #[test]
    fn missing_file_is_absence_not_failure() {
        let source = FileSource::at("/no/such/path/.credentials.json");
        assert_eq!(source.read(), Ok(None));
    }
}
