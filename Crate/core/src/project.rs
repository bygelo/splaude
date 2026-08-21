//! Project-aware recogniser bias.
//!
//! The IDE extension biases its recogniser with the workspace it is open in.
//! From the shipped bundle (`anthropic.claude-code-2.1.98`) that amounts to two
//! sources — the basename of `cwd`, and the words in the current git branch —
//! appended to the static developer list as extra `keyterms` parameters.
//!
//! splaude has no workspace: it is a menu bar app, and the field it types into
//! belongs to whatever app is frontmost. So the project is inferred instead,
//! from the session log Claude Code itself writes. Every session under
//! `~/.claude/projects/<encoded>/<uuid>.jsonl` records the directory it was
//! started in, and the newest of those is, by a wide margin, the thing the user
//! is talking about. The directory name in that path is *not* used to recover
//! the directory — the encoding collapses `/`, `.` and `_` all to `-` and is
//! not invertible — the `cwd` field inside the file is read instead.
//!
//! Having paid for the lookup, this harvests more than the extension does. The
//! wire budget is 1024 characters and the extension spends about twenty of
//! them; crate and package names, top-level directory names and the identifiers
//! a README puts in backticks are exactly the vocabulary a dictation about this
//! project will use, and they are free.
//!
//! Two sources beyond the current project earn their place, because the words a
//! dictation gets wrong are rarely inside the file you have open:
//!
//! - **Recent projects.** The same scan that finds the newest session already
//!   knows the next hundred. Their names are the repos you actually talk about,
//!   ranked by when you last touched them, and they cost nothing extra to read.
//! - **A catalog file.** Machines with many deployed things usually have an
//!   inventory of them somewhere — hosts, sites, databases, repos that live
//!   only on a server. [`catalog_keyterm`] reads any JSON file and harvests the
//!   values under name-like keys, so pointing at one is a path in the setting
//!   rather than a parser per tool.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::diagnostic;

/// Session logs older than this are assumed stale — a machine that has not run
/// `claude` in a week should not have last week's repo biasing today's speech.
const MAX_SESSION_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// How far into a session log to look for the `cwd` field. It appears within
/// the first handful of entries; reading the whole file would mean reading a
/// transcript that can run to megabytes.
const CWD_SCAN_LINE: usize = 40;

/// Caps on what any one source may contribute, so a repo with a thousand
/// top-level directories cannot crowd out the project name itself.
const MAX_DIRECTORY: usize = 24;
const MAX_README_TERM: usize = 60;

/// A term shorter than this is noise (`is`, `go`, `to`) and a term longer than
/// this is prose, not a word the recogniser needs help with. The extension uses
/// the same shape of bound.
const MIN_TERM_LENGTH: usize = 3;
const MAX_TERM_LENGTH: usize = 20;
/// The project name is allowed to be longer, matching the extension.
const MAX_NAME_LENGTH: usize = 50;

/// Words that survive every other filter and are still worth nothing.
///
/// A README is full of these — `src`, `env`, `true`, `com` — and a recogniser
/// has no trouble with any of them. They are dropped not because they are wrong
/// but because the budget they spend belongs to a word like `bygelo`.
const STOP_WORD: [&str; 32] = [
    "src", "lib", "bin", "env", "var", "tmp", "com", "org", "net", "www", "the", "and", "for",
    "with", "true", "false", "null", "new", "get", "set", "put", "type", "name", "value", "file",
    "path", "data", "text", "code", "run", "use", "add",
];

/// The directory a dictation is probably about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
}

/// The most recently active Claude Code project on this machine.
///
/// `None` when Claude Code has never run here, when its log directory is
/// unreadable, or when every session in it is older than [`MAX_SESSION_AGE`].
pub fn active() -> Option<Project> {
    active_within(&session_root()?, SystemTime::now())
}

/// `~/.claude/projects`.
fn session_root() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

/// Split out from [`active`] so the whole resolution can be driven against a
/// fixture directory and a fixed clock.
pub fn active_within(root: &Path, now: SystemTime) -> Option<Project> {
    for (_, session) in project_by_recency(root, now) {
        let Some(cwd) = read_cwd(&session) else {
            continue;
        };
        let Some(name) = cwd.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if name.len() < MIN_TERM_LENGTH || name.len() > MAX_NAME_LENGTH {
            continue;
        }
        return Some(Project {
            root: cwd.clone(),
            name: name.to_string(),
        });
    }
    None
}

/// The projects most recently worked in, newest first, `limit` at most.
///
/// Deliberately *not* every project on the machine. A hundred repo names would
/// spend the whole wire budget on things this dictation is not about; the ones
/// touched this week are the ones whose names come up.
pub fn recent_name(limit: usize) -> Vec<String> {
    match session_root() {
        Some(root) => recent_name_within(&root, SystemTime::now(), limit),
        None => Vec::new(),
    }
}

/// Split out so the ranking can be driven against a fixture directory.
pub fn recent_name_within(root: &Path, now: SystemTime, limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut name = Vec::new();

    for (_, session) in project_by_recency(root, now) {
        let Some(cwd) = read_cwd(&session) else {
            continue;
        };
        let Some(found) = cwd.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if found.len() < MIN_TERM_LENGTH || found.len() > MAX_NAME_LENGTH {
            continue;
        }
        if seen.insert(found.to_ascii_lowercase()) {
            name.push(found.to_string());
        }
        if name.len() >= limit {
            break;
        }
    }

    name
}

/// Project directories under `root`, newest first, one session file each.
///
/// Ranked by the *directory's* own modification time, which the filesystem
/// bumps whenever a session inside it is written — so one `stat` per project
/// answers "when was this repo last touched" without reading the thousands of
/// session files underneath. And because the path encoding maps one `cwd` to
/// one directory, every session in a directory shares that `cwd`: reading any
/// one of them is enough, so this hands back a single file per project.
fn project_by_recency(root: &Path, now: SystemTime) -> Vec<(SystemTime, PathBuf)> {
    let mut found = Vec::new();

    let Ok(project) = fs::read_dir(root) else {
        return found;
    };

    for project in project.flatten() {
        let Ok(modified) = project.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > MAX_SESSION_AGE {
            continue;
        }

        // The first `.jsonl` in the directory. Any one yields the project's
        // `cwd`; the newest is not needed, so this skips stat-ing the rest.
        let Ok(session) = fs::read_dir(project.path()) else {
            continue;
        };
        let Some(entry) = session.flatten().find(|entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
        }) else {
            continue;
        };

        found.push((modified, entry.path()));
    }

    found.sort_by(|left, right| right.0.cmp(&left.0));
    found
}

/// How many bytes of a session log to read looking for `cwd`.
///
/// The field is in the first handful of lines, but a transcript can run to
/// megabytes, and reading one whole just to reach line five — across every
/// project directory on the machine — is seconds on the take path.
const CWD_READ_BYTE: usize = 64 * 1024;

/// The `cwd` recorded in a session log's opening entries.
///
/// Reads a bounded prefix, not the whole file, so a multi-megabyte transcript
/// costs the same as a fresh one.
fn read_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::Read;

    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; CWD_READ_BYTE];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    let text = String::from_utf8_lossy(&buffer);

    for line in text.lines().take(CWD_SCAN_LINE) {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Only the top-level field. `cwd` also appears nested inside hook
        // payloads and tool results, where it may name a subdirectory the
        // session merely touched rather than the project root.
        if let Some(cwd) = entry.get("cwd").and_then(|value| value.as_str()) {
            return Some(PathBuf::from(cwd));
        }
    }

    None
}

/// How many recent project names may spend budget. Twenty covers about a week
/// of work on this machine and costs roughly 150 characters of the 1024.
const RECENT_LIMIT: usize = 20;

/// Characters the house tier may spend, of the 1024 on the wire.
///
/// A cap rather than a count: a machine with two hundred catalog entries would
/// otherwise fill the budget on its own and the current project's own
/// vocabulary — the file names and API names in the repo actually open — would
/// never ship at all. Half the budget to the house, leaving room for the
/// identity and builtin tiers ahead of it and the vocabulary tier behind.
const HOUSE_CHAR_BUDGET: usize = 512;

/// Recogniser bias harvested from a project directory, most specific first.
///
/// Order is load-bearing: [`pack_keyterm`](crate::speech::anthropic::pack_keyterm)
/// truncates at the wire budget rather than sampling, so whatever leads this
/// list is what survives a repo with a large README.
pub fn keyterm(project: &Project) -> Vec<String> {
    [identity(project), vocabulary(project)].concat()
}

/// What this project is called: its own name, its branch, its package. The
/// terms a dictation about it is most certain to contain.
fn identity(project: &Project) -> Vec<String> {
    let mut term = Term::new();

    term.push_name(&project.name);
    term.extend_token(&project.name);

    if let Some(branch) = branch(&project.root) {
        term.extend_token(&branch);
    }

    for name in package_name(&project.root) {
        term.push_name(&name);
        term.extend_token(&name);
    }

    term.into_inner()
}

/// What this project talks about: its directories, and the identifiers its
/// README puts in backticks. Real vocabulary, but the tail of it is noise, so
/// it ranks below anything that is a proper noun elsewhere on the machine.
fn vocabulary(project: &Project) -> Vec<String> {
    let mut term = Term::new();

    for name in top_level_directory(&project.root) {
        term.extend_token(&name);
    }

    for found in readme_term(&project.root) {
        term.extend_token(&found);
    }

    term.into_inner()
}

/// The harvested bias, split where the caller must interleave its own terms.
///
/// The ranking is the whole design, and it is not one list because the builtin
/// developer vocabulary belongs *between* these two: a dictation is most likely
/// to contain the name of the repo it is about, then ordinary programming words,
/// then the names of the other repos and hosts on this machine, and only last
/// the jargon inside the current README. Returning one flat list let a machine
/// with two hundred catalog entries evict `TypeScript` and `OAuth`, which is a
/// worse trade than dropping the tail of an inventory.
#[derive(Debug, Default, Clone)]
pub struct Harvest {
    /// This project's own name, branch and package.
    pub identity: Vec<String>,
    /// Recently worked-in projects, then the machine's catalog.
    pub house: Vec<String>,
    /// This project's directories and README identifiers.
    pub vocabulary: Vec<String>,
}

pub fn harvest(catalog: Option<&Path>) -> Harvest {
    let project = active();

    let identity = project.as_ref().map(identity).unwrap_or_default();

    let mut house = Term::new();
    for name in recent_name(RECENT_LIMIT) {
        house.push_catalog_name(&name);
    }
    for name in catalog_keyterm(catalog) {
        house.insert(&name);
    }

    Harvest {
        identity,
        house: clip(house.into_inner(), HOUSE_CHAR_BUDGET),
        vocabulary: project.as_ref().map(vocabulary).unwrap_or_default(),
    }
}

/// Keeps the leading terms that fit inside `budget` characters, commas counted
/// the way the packer counts them.
fn clip(term: Vec<String>, budget: usize) -> Vec<String> {
    let mut length = 0;
    let mut kept = Vec::new();

    for found in term {
        let cost = found.chars().count() + usize::from(!kept.is_empty());
        if length + cost > budget {
            break;
        }
        length += cost;
        kept.push(found);
    }

    kept
}

/// The active branch, read from `.git/HEAD` rather than shelled out to `git`.
///
/// The extension runs `git rev-parse --abbrev-ref HEAD`, which costs a process
/// spawn on the hotkey path and needs git on `PATH`. `.git/HEAD` is one line of
/// plain text and says the same thing. A detached head has no branch name to
/// harvest, which is the same case the extension skips as `"HEAD"`.
fn branch(root: &Path) -> Option<String> {
    let head = fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref: refs/heads/")?;
    (!reference.is_empty()).then(|| reference.to_string())
}

/// The declared package name from whichever manifest the project has.
///
/// Parsed by line rather than with a TOML dependency: every one of these is a
/// `name` key with a quoted string value, and the first such key in the file is
/// the package's own. A workspace root without a `[package]` section yields
/// nothing, which is correct — there is no single name to take.
fn package_name(root: &Path) -> Vec<String> {
    let mut found = Vec::new();

    for manifest in ["Cargo.toml", "pyproject.toml"] {
        let Ok(text) = fs::read_to_string(root.join(manifest)) else {
            continue;
        };
        let mut in_package = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_package = line == "[package]" || line == "[project]";
                continue;
            }
            if !in_package {
                continue;
            }
            if let Some(value) = line.strip_prefix("name") {
                if let Some(name) = quoted(value) {
                    found.push(name);
                    break;
                }
            }
        }
    }

    if let Ok(text) = fs::read_to_string(root.join("package.json")) {
        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = manifest.get("name").and_then(|value| value.as_str()) {
                // Scoped packages carry the org in the name; the bare package
                // is the half anyone says out loud.
                found.push(name.rsplit('/').next().unwrap_or(name).to_string());
            }
        }
    }

    found
}

/// The string inside the first pair of quotes, if any.
fn quoted(value: &str) -> Option<String> {
    let (_, rest) = value.split_once(['"', '\''])?;
    let (inner, _) = rest.split_once(['"', '\''])?;
    (!inner.is_empty()).then(|| inner.to_string())
}

/// Top-level directory names, minus the ones every project has.
fn top_level_directory(root: &Path) -> Vec<String> {
    const IGNORED: [&str; 12] = [
        "node_modules",
        "target",
        "build",
        "dist",
        "out",
        "vendor",
        "venv",
        "__pycache__",
        "coverage",
        "tmp",
        "temp",
        "bin",
    ];

    let Ok(entry) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for found_entry in entry.flatten() {
        if !found_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = found_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // Dotted directories are tooling, not vocabulary.
        if name.starts_with('.') || IGNORED.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        found.push(name);
        if found.len() >= MAX_DIRECTORY {
            break;
        }
    }

    found
}

/// Identifiers a README puts in backticks, plus the words in its headings.
///
/// Backticked spans are the highest-value source in the whole harvester: a
/// README wraps exactly the file names, commands, flags and API names that a
/// general recogniser has never seen and a dictation about the project will
/// say out loud.
fn readme_term(root: &Path) -> Vec<String> {
    let Some(text) = ["README.md", "README", "readme.md"]
        .iter()
        .find_map(|name| fs::read_to_string(root.join(name)).ok())
    else {
        return Vec::new();
    };

    let mut found = Vec::new();

    // Backtick spans. Fences open and close with the same character, so a
    // fenced block reads as one enormous "span" — length bounds discard it.
    for (index, chunk) in text.split('`').enumerate() {
        if index % 2 == 1 && chunk.len() <= 120 && !chunk.contains('\n') {
            found.push(chunk.to_string());
        }
    }

    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            found.push(line.trim_start_matches(['#', ' ']).to_string());
        }
    }

    found.truncate(MAX_README_TERM);
    found
}

// MARK: - Catalog

/// Keys whose string values name a thing worth biasing toward.
///
/// An inventory of a machine's infrastructure is a nest of lists of objects,
/// and the proper nouns in it — a host, a site, a database, a repo that only
/// exists on a server — always sit under a key from this set. Harvesting by key
/// rather than by schema means any such file works without a parser per tool.
const CATALOG_NAME_KEY: [&str; 6] = ["name", "project", "host_code", "slug", "host", "alias"];

/// Refuse to read a catalog larger than this. A runaway inventory should cost a
/// skipped bias, not a dictation that stalls while megabytes are parsed.
const CATALOG_BYTE_LIMIT: u64 = 8 * 1024 * 1024;

/// Well-known catalog locations, probed when the setting names none.
///
/// Only `booted` so far — it caches its scan to a plain JSON file, which is why
/// this reads a file rather than the `:5173` endpoint that serves the same
/// data. A file read cannot hang on the hotkey path, and splaude has no
/// business shipping an HTTP client that talks to whatever a setting points at.
fn known_catalog() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![home.join(".booted").join("inventory.json")]
}

/// Names harvested from a JSON catalog of the machine's own infrastructure.
///
/// Ordered as the file is: an inventory is usually written newest-or-most-used
/// first, and truncation at the wire budget makes that ordering matter.
pub fn catalog_keyterm(path: Option<&Path>) -> Vec<String> {
    let candidate: Vec<PathBuf> = match path {
        Some(named) => vec![named.to_path_buf()],
        None => known_catalog(),
    };

    for path in candidate {
        if fs::metadata(&path)
            .map(|meta| meta.len())
            .unwrap_or(u64::MAX)
            > CATALOG_BYTE_LIMIT
        {
            diagnostic::log("project", format!("catalog too large: {}", path.display()));
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            diagnostic::log(
                "project",
                format!("catalog is not JSON: {}", path.display()),
            );
            continue;
        };

        let mut term = Term::new();
        harvest_name(&value, &mut term);
        let found = term.into_inner();
        diagnostic::log(
            "project",
            format!("catalog {} — {} name", path.display(), found.len()),
        );
        return found;
    }

    Vec::new()
}

/// Walks any JSON shape, keeping the string values under [`CATALOG_NAME_KEY`].
fn harvest_name(value: &serde_json::Value, term: &mut Term) {
    match value {
        serde_json::Value::Object(field) => {
            // An object with a `pid` is a running process, and its `name` is an
            // OS process name — `node`, `rapportd`, `ControlCe` truncated by the
            // kernel. None of that is a word anyone dictates, and on a busy
            // machine there is more of it than there are repos.
            let is_process = field.contains_key("pid");

            for (key, inner) in field {
                if is_process && key == "name" {
                    continue;
                }
                if CATALOG_NAME_KEY.contains(&key.as_str()) {
                    if let Some(found) = inner.as_str() {
                        // A name is one word or a short hyphenated one. A
                        // sentence under a `name` key is a description.
                        if !found.contains(' ') {
                            term.push_catalog_name(found);
                        }
                    }
                }
                harvest_name(inner, term);
            }
        }
        serde_json::Value::Array(item) => {
            for inner in item {
                harvest_name(inner, term);
            }
        }
        _ => {}
    }
}

/// An ordered, deduped term list. Deduplication is case-insensitive so
/// `splaude` and `Splaude` do not both spend budget.
struct Term {
    seen: HashSet<String>,
    kept: Vec<String>,
}

impl Term {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            kept: Vec::new(),
        }
    }

    /// A whole name, kept as-is and allowed the longer bound.
    fn push_name(&mut self, value: &str) {
        let value = value.trim().trim_matches(['-', '_', '.']);
        if value.len() < MIN_TERM_LENGTH || value.len() > MAX_NAME_LENGTH {
            return;
        }
        if digit_heavy(value) || STOP_WORD.contains(&value.to_ascii_lowercase().as_str()) {
            return;
        }
        self.insert(value);
    }

    /// A catalog name: the whole thing, plus its leading segment.
    ///
    /// Deliberately *not* the full token split the rest of the harvester uses.
    /// An inventory is mostly `{project}-{role}` pairs, and splitting them
    /// yields `api`, `web`, `app`, `auth`, `site` — words a recogniser already
    /// knows perfectly and which spend budget that `fourlinq` and `bygelo`
    /// needed. The leading segment is the project, which is the half worth
    /// having on its own.
    fn push_catalog_name(&mut self, value: &str) {
        self.push_name(value);
        if let Some((lead, _)) = value.split_once(['-', '_', '.']) {
            self.push_name(lead);
        }
    }

    /// Splits an identifier into the words someone would say, and keeps each.
    ///
    /// `rust-workspace` is two spoken words, and so is `speechBackend`; the
    /// recogniser is helped by the parts, not by the joined form it will never
    /// hear. The joined form is kept too when it is pronounceable as one word.
    fn extend_token(&mut self, value: &str) {
        for word in split_identifier(value) {
            if word.len() < MIN_TERM_LENGTH || word.len() > MAX_TERM_LENGTH {
                continue;
            }
            // A README is full of version numbers, ports and hex colours.
            // Nobody dictates `E8763` or `600`, and a recogniser biased toward
            // them will hear them in noise. A digit inside a word is fine —
            // `nova3` and `linear16` are said out loud — so this is a ratio and
            // not a ban.
            if digit_heavy(&word) || STOP_WORD.contains(&word.to_ascii_lowercase().as_str()) {
                continue;
            }
            self.insert(&word);
        }
    }

    fn insert(&mut self, value: &str) {
        let key = value.to_ascii_lowercase();
        if self.seen.insert(key) {
            self.kept.push(value.to_string());
        }
    }

    fn into_inner(self) -> Vec<String> {
        self.kept
    }
}

/// Whether a token is mostly digits, and so an identifier nobody pronounces.
fn digit_heavy(value: &str) -> bool {
    let total = value.chars().count();
    let digit = value
        .chars()
        .filter(|character| character.is_numeric())
        .count();
    total == 0 || digit * 5 >= total * 2
}

/// `camelCase`, `PascalCase`, `snake_case`, `kebab-case` and paths, all split
/// into their words. An acronym run stays whole: `HTTPServer` is `HTTP` and
/// `Server`, not `H`, `T`, `T`, `P` and `Server`.
fn split_identifier(value: &str) -> Vec<String> {
    let mut word = Vec::new();
    let mut current = String::new();

    let character: Vec<char> = value.chars().collect();
    for (index, &this) in character.iter().enumerate() {
        if !this.is_alphanumeric() {
            if !current.is_empty() {
                word.push(std::mem::take(&mut current));
            }
            continue;
        }

        let previous = index.checked_sub(1).map(|at| character[at]);
        let next = character.get(index + 1).copied();

        // A capital starts a new word when it follows a lowercase letter
        // (`fooBar`) or begins one inside an acronym run (`HTTPServer`).
        let boundary = this.is_uppercase()
            && match (previous, next) {
                (Some(previous), _) if previous.is_lowercase() || previous.is_numeric() => true,
                (Some(previous), Some(next)) => previous.is_uppercase() && next.is_lowercase(),
                _ => false,
            };

        if boundary && !current.is_empty() {
            word.push(std::mem::take(&mut current));
        }
        current.push(this);
    }

    if !current.is_empty() {
        word.push(current);
    }

    // The joined form is worth keeping only when it was a single word to begin
    // with — otherwise it is a spelling no one pronounces.
    if word.len() > 1 && value.chars().all(char::is_alphanumeric) {
        word.push(value.to_string());
    }

    word
}

/// The active project's bias terms, recomputed at most once every
/// [`CACHE_TTL`].
///
/// A take starts on a hotkey press and the socket opens immediately, so the
/// harvest sits on the latency path. Reading a README and a directory listing
/// is sub-millisecond, but doing it on every press for a repo that has not
/// changed is waste — and the answer only moves when the user switches project
/// or branch, which is not a per-take event.
pub fn cached_harvest(catalog: Option<&Path>) -> Harvest {
    let now = SystemTime::now();

    let (current, stale) = {
        let cache = lock_cache();
        let stale = cache
            .value
            .as_ref()
            .map(|(at, _)| now.duration_since(*at).unwrap_or_default() >= CACHE_TTL)
            .unwrap_or(true);
        (cache.value.as_ref().map(|(_, found)| found.clone()), stale)
    };

    // The refresh runs on its own thread and a take reads whatever is cached
    // right now — possibly stale, possibly empty on the first ever take. The
    // harvest walks every project directory on the machine (thousands of files,
    // gigabytes); doing that on the hotkey path put whole seconds between the
    // keypress and the microphone. The next take gets the fresh list; this one
    // never waits.
    if stale {
        spawn_refresh(catalog.map(|path| path.to_path_buf()));
    }

    current.unwrap_or_default()
}

/// Warm the cache before the first take, so even that one carries the bias.
///
/// Called once at launch. Without it the first dictation after opening the app
/// ships no project terms, because the background refresh it triggers has not
/// finished yet.
pub fn warm(catalog: Option<&Path>) {
    let _ = cached_harvest(catalog);
}

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

struct Cache {
    value: Option<(SystemTime, Harvest)>,
    refreshing: bool,
}

fn lock_cache() -> std::sync::MutexGuard<'static, Cache> {
    use std::sync::Mutex;
    static CACHE: Mutex<Cache> = Mutex::new(Cache {
        value: None,
        refreshing: false,
    });
    // A poisoned lock means a previous harvest panicked; the fallback is to use
    // it anyway, never to lose dictation over a cache.
    match CACHE.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Recompute the harvest on a background thread, unless one is already running.
fn spawn_refresh(catalog: Option<PathBuf>) {
    {
        let mut cache = lock_cache();
        if cache.refreshing {
            return;
        }
        cache.refreshing = true;
    }

    std::thread::spawn(move || {
        let found = harvest(catalog.as_deref());
        diagnostic::log(
            "project",
            format!(
                "harvest — {} identity, {} house, {} vocabulary",
                found.identity.len(),
                found.house.len(),
                found.vocabulary.len()
            ),
        );
        let mut cache = lock_cache();
        cache.value = Some((SystemTime::now(), found));
        cache.refreshing = false;
    });
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn splits_a_kebab_identifier() {
        assert_eq!(split_identifier("rust-workspace"), ["rust", "workspace"]);
    }

    #[test]
    fn splits_camel_case_and_keeps_the_joined_form() {
        assert_eq!(
            split_identifier("speechBackend"),
            ["speech", "Backend", "speechBackend"]
        );
    }

    #[test]
    fn keeps_an_acronym_run_whole() {
        // The failure this guards is `HTTPServer` arriving as five terms of one
        // letter each, which is budget spent on nothing.
        assert_eq!(
            split_identifier("HTTPServer"),
            ["HTTP", "Server", "HTTPServer"]
        );
    }

    #[test]
    fn splits_a_path() {
        assert_eq!(split_identifier("Crate/core/src"), ["Crate", "core", "src"]);
    }

    #[test]
    fn does_not_invent_a_joined_form_for_a_separated_identifier() {
        // `rust-workspace` is never said as one word, so shipping it as a term
        // would only teach the recogniser a spelling it will never hear.
        assert!(!split_identifier("rust-workspace").contains(&"rust-workspace".to_string()));
    }

    #[test]
    fn digit_heavy_drops_a_number_but_keeps_a_pronounced_identifier() {
        assert!(digit_heavy("600"));
        assert!(digit_heavy("E8763"));
        assert!(!digit_heavy("nova3"));
        assert!(!digit_heavy("linear16"));
        assert!(!digit_heavy("splaude"));
    }

    #[test]
    fn term_dedupes_case_insensitively() {
        let mut term = Term::new();
        term.push_name("splaude");
        term.push_name("Splaude");
        assert_eq!(term.into_inner(), ["splaude"]);
    }

    #[test]
    fn term_drops_a_word_too_short_to_be_worth_biasing() {
        let mut term = Term::new();
        term.extend_token("go-to-ui");
        assert!(term.into_inner().is_empty());
    }

    #[test]
    fn quoted_reads_the_first_string() {
        assert_eq!(quoted(r#" = "splaude-core""#).unwrap(), "splaude-core");
    }

    #[test]
    fn harvests_a_real_project_tree() {
        let root = std::env::temp_dir().join("splaude-project-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("Source")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(
            root.join(".git").join("HEAD"),
            "ref: refs/heads/rust-workspace\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmember = []\n\n[package]\nname = \"splaude-core\"\n",
        )
        .unwrap();
        fs::write(
            root.join("README.md"),
            "# splaude\n\nUse `pack_keyterm` first.\n",
        )
        .unwrap();

        let term = keyterm(&Project {
            root: root.clone(),
            name: "splaude".into(),
        });

        assert_eq!(term.first().unwrap(), "splaude");
        for expected in [
            "rust",
            "workspace",
            "splaude-core",
            "core",
            "Source",
            "pack",
            "keyterm",
        ] {
            assert!(
                term.contains(&expected.to_string()),
                "missing {expected}: {term:?}"
            );
        }
        // Ignored directories must not spend budget.
        assert!(!term.iter().any(|value| value == "node_modules"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolves_the_newest_session_to_its_recorded_cwd() {
        let root = std::env::temp_dir().join("splaude-session-test");
        let _ = fs::remove_dir_all(&root);
        let project = root.join("-Users-someone-Antigravity-splaude");
        fs::create_dir_all(&project).unwrap();
        // The directory name is deliberately not invertible back to the path —
        // this asserts the `cwd` field is what gets read.
        fs::write(
            project.join("a.jsonl"),
            "{\"type\":\"mode\"}\n{\"cwd\":\"/Users/someone/Antigravity/sisia_app\"}\n",
        )
        .unwrap();

        let found = active_within(&root, SystemTime::now()).unwrap();
        assert_eq!(found.name, "sisia_app");
        assert_eq!(
            found.root,
            PathBuf::from("/Users/someone/Antigravity/sisia_app")
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn recent_name_is_ordered_newest_first_and_deduped() {
        let root = std::env::temp_dir().join("splaude-recent-test");
        let _ = fs::remove_dir_all(&root);

        // Two session directories for the same repo plus one for another; the
        // repo must appear once, at the rank of its newest session.
        for (dir, cwd) in [("-a", "/x/blead"), ("-b", "/x/booted"), ("-c", "/x/blead")] {
            let project = root.join(dir);
            fs::create_dir_all(&project).unwrap();
            fs::write(project.join("s.jsonl"), format!("{{\"cwd\":\"{cwd}\"}}\n")).unwrap();
            // mtime ordering is what ranks these, and the filesystem's
            // resolution is coarse enough that same-millisecond writes tie.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let found = recent_name_within(&root, SystemTime::now(), 10);
        assert_eq!(found, ["blead", "booted"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn recent_name_honours_the_limit() {
        let root = std::env::temp_dir().join("splaude-limit-test");
        let _ = fs::remove_dir_all(&root);
        for index in 0..5 {
            let project = root.join(format!("-{index}"));
            fs::create_dir_all(&project).unwrap();
            fs::write(
                project.join("s.jsonl"),
                format!("{{\"cwd\":\"/x/repo{index}name\"}}\n"),
            )
            .unwrap();
        }
        assert_eq!(recent_name_within(&root, SystemTime::now(), 2).len(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn catalog_harvests_names_and_skips_running_processes() {
        let path = std::env::temp_dir().join("splaude-catalog-test.json");
        fs::write(
            &path,
            r#"{
              "repo": [{"name": "fourlinq-hr"}, {"name": "bygelo"}],
              "vps": [{"host": "advo"}],
              "process": [{"pid": 42, "name": "rapportd", "port": 5432}],
              "note": {"name": "a whole sentence under a name key"}
            }"#,
        )
        .unwrap();

        let found = catalog_keyterm(Some(&path));

        assert!(found.contains(&"fourlinq-hr".to_string()));
        // The leading segment ships too — `fourlinq` is said on its own.
        assert!(found.contains(&"fourlinq".to_string()));
        assert!(found.contains(&"bygelo".to_string()));
        assert!(found.contains(&"advo".to_string()));
        // A process name is an OS artefact, not a word anyone dictates.
        assert!(!found.contains(&"rapportd".to_string()));
        // `hr` never ships alone: catalog names are not token-split, which is
        // what keeps `api`, `web` and `auth` out of the budget.
        assert!(!found.iter().any(|term| term == "hr"));
        assert!(!found.iter().any(|term| term.contains(' ')));

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn catalog_is_silent_about_a_file_that_is_not_json() {
        let path = std::env::temp_dir().join("splaude-catalog-bad.json");
        fs::write(&path, "not json").unwrap();
        assert!(catalog_keyterm(Some(&path)).is_empty());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn clip_stops_at_the_budget_counting_separators() {
        let term = ["aaaa", "bbbb", "cccc"].map(String::from).to_vec();
        // "aaaa,bbbb" is 9 characters; one more term would be 14.
        assert_eq!(clip(term, 9), ["aaaa", "bbbb"]);
    }

    #[test]
    fn a_stop_word_never_spends_budget() {
        let mut term = Term::new();
        term.extend_token("src/lib/env");
        assert!(term.into_inner().is_empty());
    }

    #[test]
    fn ignores_a_session_older_than_the_staleness_bound() {
        let root = std::env::temp_dir().join("splaude-stale-test");
        let _ = fs::remove_dir_all(&root);
        let project = root.join("-old");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("a.jsonl"), "{\"cwd\":\"/tmp/old\"}\n").unwrap();

        let later = SystemTime::now() + MAX_SESSION_AGE + std::time::Duration::from_secs(60);
        assert!(active_within(&root, later).is_none());

        fs::remove_dir_all(&root).unwrap();
    }
}
