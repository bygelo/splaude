//! Whether a newer splaude has been published.
//!
//! This module checks and reports. It does not download, replace or restart
//! anything, and that is a deliberate stopping point rather than an unfinished
//! one: installing an update over the running binary is a different problem on
//! each platform, and on macOS it is currently a harmful one. An ad-hoc
//! signature changes identity on every release, and macOS keys Accessibility
//! and Microphone grants to that identity — so a Mac that updated itself would
//! silently lose permission to do the only thing it exists to do. Telling
//! someone a new version exists costs none of that.
//!
//! The rate limit is the other reason to keep this small. GitHub allows 60
//! unauthenticated requests an hour per address, and splaude sends no token, so
//! the check is worth making rarely and worth failing quietly.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use ureq::Agent;

/// Where the published release lives. Not configurable on purpose — an update
/// check that can be pointed somewhere else by editing a JSON file is a way to
/// hand someone a different binary.
pub const ENDPOINT: &str = "https://api.github.com/repos/bygelo/splaude/releases/latest";

/// GitHub rejects an unauthenticated API request that does not identify itself.
pub const USER_AGENT: &str = concat!("splaude/", env!("CARGO_PKG_VERSION"));

/// The version this build reports, for comparison against the published one.
pub fn current() -> Version {
    // Parsed rather than const because `CARGO_PKG_VERSION` is a string, and a
    // build whose own version does not parse should fail its test, not ship a
    // check that silently never fires.
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// A three-number version, which is all splaude has ever tagged.
///
/// Deliberately not the `semver` crate. The only versions compared here are the
/// ones this project publishes, the comparison is a tuple ordering, and a
/// dependency earning its place on `<` alone is a stretch. What the hand-rolled
/// parse must not do is *guess*: anything carrying a pre-release or build
/// suffix is rejected rather than truncated to its numbers, because `0.3.0-rc1`
/// silently becoming `0.3.0` would advertise a release candidate as a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses `0.2.0` or `v0.2.0`. Tags carry the `v`; `CARGO_PKG_VERSION` does
    /// not, and both reach this function.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        let body = trimmed.strip_prefix('v').unwrap_or(trimmed);

        // Rejected rather than split on: see the type's note on `0.3.0-rc1`.
        if body.contains('-') || body.contains('+') {
            return None;
        }

        let mut part = body.split('.');
        let major = part.next()?.parse().ok()?;
        let minor = part.next()?.parse().ok()?;
        let patch = part.next()?.parse().ok()?;
        // A fourth component means this is not the shape we think it is.
        if part.next().is_some() {
            return None;
        }

        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A published release, as much of it as this check cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The human page, not an asset. What the menu opens.
    pub url: String,
}

/// The subset of GitHub's release JSON that matters here.
///
/// `/releases/latest` already excludes drafts and pre-releases, so neither flag
/// needs reading — but the version parse rejects a pre-release suffix anyway,
/// which means a change at their end cannot turn into an upgrade prompt here.
#[derive(Deserialize)]
struct Payload {
    tag_name: String,
    html_url: String,
}

/// Parses the API response. Pure, so the shape of GitHub's answer can be
/// pinned by a test rather than by making a request.
pub fn parse(body: &[u8]) -> Result<Release, String> {
    let payload: Payload =
        serde_json::from_slice(body).map_err(|why| format!("unreadable answer: {why}"))?;

    let version = Version::parse(&payload.tag_name)
        .ok_or_else(|| format!("unrecognised tag: {}", payload.tag_name))?;

    Ok(Release {
        version,
        url: payload.html_url,
    })
}

/// How long to wait before deciding the check is not going to answer. Short on
/// purpose: nobody is blocked on this, and a menu that takes half a minute to
/// open because a network is unreachable is a worse bug than a missed update.
const PATIENCE: Duration = Duration::from_secs(10);

/// A ceiling on the answer this will read into memory. The real payload is a
/// couple of kilobytes; anything beyond this is not the API answering.
const CEILING: u64 = 256 * 1024;

/// Asks GitHub what the newest release is.
///
/// Blocking, and belongs on a thread that is allowed to block. The caller is
/// the app's runtime, which has one — this stays synchronous because it is a
/// single request with no streaming, and an async client would put a second
/// HTTP stack in the binary to save nothing.
///
/// No crypto provider is installed here. rustls resolves its provider from the
/// features compiled in, and `ring` is the only one in this graph; the speech
/// backend additionally installs it explicitly, and doing so twice is a no-op.
/// If a second provider ever arrives, that install becomes load-bearing for
/// this path too — which is why it is worth saying here rather than leaving the
/// coupling implicit.
pub fn fetch() -> Result<Release, String> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(PATIENCE))
        .user_agent(USER_AGENT)
        .build()
        .into();

    let mut response = agent
        .get(ENDPOINT)
        // Without this GitHub is free to answer with whatever it defaults to.
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|why| why.to_string())?;

    let body = response
        .body_mut()
        .with_config()
        .limit(CEILING)
        .read_to_vec()
        .map_err(|why| format!("could not read the answer: {why}"))?;

    parse(&body)
}

/// The whole check: ask, compare, and say what it amounts to.
///
/// Returns a [`Reading`] rather than a `Result` because every outcome including
/// failure is something the UI shows rather than something a caller handles.
pub fn check() -> Reading {
    match fetch() {
        Ok(release) => compare(current(), release),
        Err(why) => Reading::Failed(why),
    }
}

/// What the check amounts to, as a value rather than a sentence.
///
/// The same discipline as [`crate::quota::Reading`]: "checked, nothing newer"
/// and "could not check" are different claims and flattening them would tell
/// someone they are up to date when the truth is that nobody knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// Nothing has been checked yet this run.
    Unknown,
    /// Checked, and this build is the newest published one — or newer, which is
    /// what a local build between releases looks like.
    Current,
    /// A newer release exists.
    Available(Release),
    /// The check itself failed. Carries why, for the log and the report.
    Failed(String),
}

impl Reading {
    /// One line for a menu item or the `--check` report.
    pub fn line(&self) -> String {
        match self {
            Reading::Unknown => "not checked yet".into(),
            Reading::Current => format!("{} is the latest", current()),
            Reading::Available(release) => format!("{} available", release.version),
            Reading::Failed(why) => format!("could not check ({why})"),
        }
    }

    /// Whether this reading is worth putting in front of someone. Everything
    /// else is answering a question nobody asked.
    pub fn is_worth_saying(&self) -> bool {
        matches!(self, Reading::Available(_))
    }
}

/// Compares a published release against the running build.
///
/// Strictly greater, never merely different: a build made from the branch after
/// a tag legitimately reports a version above the newest release, and telling
/// that person to "update" to an older binary would be wrong.
pub fn compare(current: Version, published: Release) -> Reading {
    if published.version > current {
        Reading::Available(published)
    } else {
        Reading::Current
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn version(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    fn release(major: u32, minor: u32, patch: u32) -> Release {
        Release {
            version: version(major, minor, patch),
            url: "https://example.invalid/release".into(),
        }
    }

    #[test]
    fn a_tag_parses_with_or_without_its_v() {
        assert_eq!(Version::parse("v0.2.0"), Some(version(0, 2, 0)));
        assert_eq!(Version::parse("0.2.0"), Some(version(0, 2, 0)));
        assert_eq!(Version::parse("  v1.20.3  "), Some(version(1, 20, 3)));
    }

    /// The failure this guards is specific: truncating `0.3.0-rc1` to `0.3.0`
    /// would advertise a release candidate as a release.
    #[test]
    fn a_prerelease_or_build_suffix_is_refused_rather_than_truncated() {
        assert_eq!(Version::parse("0.3.0-rc1"), None);
        assert_eq!(Version::parse("v0.3.0-beta.2"), None);
        assert_eq!(Version::parse("0.3.0+build7"), None);
    }

    #[test]
    fn a_tag_that_is_not_three_numbers_is_refused() {
        assert_eq!(Version::parse("0.2"), None);
        assert_eq!(Version::parse("0.2.0.1"), None);
        assert_eq!(Version::parse("latest"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v"), None);
    }

    #[test]
    fn ordering_compares_each_component_in_turn() {
        assert!(version(0, 3, 0) > version(0, 2, 9));
        assert!(version(1, 0, 0) > version(0, 99, 99));
        assert!(version(0, 2, 10) > version(0, 2, 9));
    }

    #[test]
    fn a_newer_release_is_available() {
        let reading = compare(version(0, 2, 0), release(0, 3, 0));
        assert_eq!(reading, Reading::Available(release(0, 3, 0)));
        assert!(reading.is_worth_saying());
    }

    #[test]
    fn the_same_version_is_current() {
        assert_eq!(
            compare(version(0, 2, 0), release(0, 2, 0)),
            Reading::Current
        );
    }

    /// A build from the branch after a tag is ahead of the newest release, and
    /// must not be told to install an older binary.
    #[test]
    fn a_build_ahead_of_the_newest_release_is_not_offered_a_downgrade() {
        assert_eq!(
            compare(version(0, 3, 0), release(0, 2, 0)),
            Reading::Current
        );
    }

    #[test]
    fn the_api_answer_yields_a_version_and_a_page() {
        let body = br#"{
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/bygelo/splaude/releases/tag/v0.3.0",
            "name": "splaude 0.3.0",
            "assets": []
        }"#;
        let parsed = parse(body).expect("this is the shape GitHub answers with");
        assert_eq!(parsed.version, version(0, 3, 0));
        assert_eq!(
            parsed.url,
            "https://github.com/bygelo/splaude/releases/tag/v0.3.0"
        );
    }

    #[test]
    fn an_unreadable_answer_is_an_error_rather_than_a_panic() {
        assert!(parse(b"not json at all").is_err());
        assert!(parse(b"{}").is_err());
    }

    /// A tag GitHub accepts but this parse does not must not become an upgrade
    /// prompt — it becomes a refusal with the tag named.
    #[test]
    fn an_unrecognised_tag_names_itself_in_the_error() {
        let body = br#"{"tag_name": "nightly", "html_url": "https://example.invalid"}"#;
        let why = parse(body).expect_err("nightly is not a version");
        assert!(
            why.contains("nightly"),
            "the error should name the tag: {why}"
        );
    }

    /// Every reading has to render, and none of them may render empty — a blank
    /// menu item is worse than a wrong one because it looks like a bug.
    #[test]
    fn no_reading_renders_empty() {
        for reading in [
            Reading::Unknown,
            Reading::Current,
            Reading::Available(release(9, 9, 9)),
            Reading::Failed("timed out".into()),
        ] {
            assert!(
                !reading.line().trim().is_empty(),
                "{reading:?} rendered empty"
            );
        }
    }

    /// Only an actual update is worth interrupting someone with.
    #[test]
    fn only_an_available_update_is_worth_saying() {
        assert!(!Reading::Unknown.is_worth_saying());
        assert!(!Reading::Current.is_worth_saying());
        assert!(!Reading::Failed("offline".into()).is_worth_saying());
        assert!(Reading::Available(release(1, 0, 0)).is_worth_saying());
    }

    /// The version this binary reports has to be one the comparison can read,
    /// or the check would quietly never fire.
    #[test]
    fn this_build_reports_a_version_that_parses() {
        assert!(
            Version::parse(env!("CARGO_PKG_VERSION")).is_some(),
            "CARGO_PKG_VERSION {} does not parse",
            env!("CARGO_PKG_VERSION")
        );
        assert_ne!(current(), version(0, 0, 0), "current() fell back to zero");
    }
}
