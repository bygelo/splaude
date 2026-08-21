import Foundation

/// Project-aware recogniser bias.
///
/// The IDE extension biases its recogniser with the workspace it is open in.
/// From the shipped bundle (`anthropic.claude-code-2.1.98`) that amounts to two
/// sources — the basename of `cwd`, and the words in the current git branch —
/// appended to the static developer list as extra `keyterms` parameters.
///
/// splaude has no workspace: it is a menu bar app, and the field it types into
/// belongs to whatever app is frontmost. So the project is inferred instead,
/// from the session log Claude Code itself writes. Every session under
/// `~/.claude/projects/<encoded>/<uuid>.jsonl` records the directory it was
/// started in, and the newest of those is, by a wide margin, the thing the user
/// is talking about. The directory name in that path is *not* used to recover
/// the directory — the encoding collapses `/`, `.` and `_` all to `-` and is
/// not invertible — the `cwd` field inside the file is read instead.
///
/// Having paid for the lookup, this harvests more than the extension does. The
/// wire budget is 1024 characters and the extension spends about twenty of
/// them. Two sources beyond the current project earn their place, because the
/// words a dictation gets wrong are rarely inside the file you have open:
///
/// - **Recent projects.** The same scan that finds the newest session already
///   knows the next hundred. Their names are the repos you actually talk about.
/// - **A catalog file.** Machines with many deployed things usually have an
///   inventory of them somewhere — hosts, sites, databases, repos that live
///   only on a server. `catalogKeyterm` reads any JSON file and harvests the
///   values under name-like keys, so pointing at one is a path in the setting
///   rather than a parser per tool.
///
/// This is a port of `Crate/core/src/project.rs`; the two must stay in step.
enum Project {

    // MARK: - Bounds

    /// Session logs older than this are assumed stale — a machine that has not
    /// run `claude` in a week should not have last week's repo biasing today's
    /// speech.
    private static let maxSessionAge: TimeInterval = 7 * 24 * 60 * 60

    /// How far into a session log to look for the `cwd` field. It appears
    /// within the first handful of entries; reading the whole file would mean
    /// reading a transcript that can run to megabytes.
    private static let cwdScanLine = 40

    /// A term shorter than this is noise (`is`, `go`, `to`) and a term longer
    /// than this is prose, not a word the recogniser needs help with.
    private static let minTermLength = 3
    private static let maxTermLength = 20
    /// A name is allowed to be longer, matching the extension.
    private static let maxNameLength = 50

    /// Caps on what any one source may contribute, so a repo with a thousand
    /// top-level directories cannot crowd out the project name itself.
    private static let maxDirectory = 24
    private static let maxReadmeTerm = 60

    /// How many recent project names may spend budget. Twenty covers about a
    /// week of work and costs roughly 150 characters of the 1024.
    private static let recentLimit = 20

    /// Characters the house tier may spend, of the 1024 on the wire.
    ///
    /// A cap rather than a count: a machine with two hundred catalog entries
    /// would otherwise fill the budget on its own and the current project's own
    /// vocabulary — the file names and API names in the repo actually open —
    /// would never ship at all.
    private static let houseCharBudget = 512

    /// Refuse to read a catalog larger than this. A runaway inventory should
    /// cost a skipped bias, not a dictation that stalls parsing megabytes.
    private static let catalogByteLimit = 8 * 1024 * 1024

    /// Keys whose string values name a thing worth biasing toward.
    ///
    /// An inventory of a machine's infrastructure is a nest of lists of
    /// objects, and the proper nouns in it — a host, a site, a database, a repo
    /// that only exists on a server — always sit under a key from this set.
    /// Harvesting by key rather than by schema means any such file works
    /// without a parser per tool.
    private static let catalogNameKey: Set<String> = [
        "name", "project", "host_code", "slug", "host", "alias",
    ]

    /// Words that survive every other filter and are still worth nothing.
    ///
    /// A README is full of these — `src`, `env`, `true`, `com` — and a
    /// recogniser has no trouble with any of them. They are dropped not because
    /// they are wrong but because the budget they spend belongs to a word like
    /// `bygelo`.
    private static let stopWord: Set<String> = [
        "src", "lib", "bin", "env", "var", "tmp", "com", "org", "net", "www",
        "the", "and", "for", "with", "true", "false", "null", "new", "get",
        "set", "put", "type", "name", "value", "file", "path", "data", "text",
        "code", "run", "use", "add",
    ]

    // MARK: - Resolution

    /// The directory a dictation is probably about.
    struct Resolved: Equatable {
        let root: URL
        let name: String
    }

    /// `~/.claude/projects`.
    private static var sessionRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude/projects")
    }

    /// The most recently active Claude Code project on this machine.
    ///
    /// `nil` when Claude Code has never run here, when its log directory is
    /// unreadable, or when every session in it is stale.
    static func active() -> Resolved? {
        activeWithin(sessionRoot, now: Date())
    }

    /// Split out from `active` so the whole resolution can be driven against a
    /// fixture directory and a fixed clock.
    static func activeWithin(_ root: URL, now: Date) -> Resolved? {
        for directory in projectByRecency(root, now: now) {
            guard let cwd = readCwd(directory.session) else { continue }
            let name = cwd.lastPathComponent
            guard name.count >= minTermLength, name.count <= maxNameLength else { continue }
            return Resolved(root: cwd, name: name)
        }
        return nil
    }

    /// The projects most recently worked in, newest first, `limit` at most.
    ///
    /// Deliberately *not* every project on the machine. A hundred repo names
    /// would spend the whole wire budget on things this dictation is not about;
    /// the ones touched this week are the ones whose names come up.
    static func recentName(_ limit: Int) -> [String] {
        recentNameWithin(sessionRoot, now: Date(), limit: limit)
    }

    /// Split out so the ranking can be driven against a fixture directory.
    static func recentNameWithin(_ root: URL, now: Date, limit: Int) -> [String] {
        var seen = Set<String>()
        var name: [String] = []

        for directory in projectByRecency(root, now: now) {
            guard let cwd = readCwd(directory.session) else { continue }
            let found = cwd.lastPathComponent
            guard found.count >= minTermLength, found.count <= maxNameLength else { continue }
            if seen.insert(found.lowercased()).inserted { name.append(found) }
            if name.count >= limit { break }
        }

        return name
    }

    /// Project directories under `root`, newest first, one session file each.
    ///
    /// Ranked by the *directory's* own modification time, which the filesystem
    /// bumps whenever a session inside it is written — so one `stat` per project
    /// answers "when was this repo last touched" without reading the thousands
    /// of session files underneath. And because the path encoding maps one `cwd`
    /// to one directory, every session in a directory shares that `cwd`: reading
    /// any one of them is enough, so this hands back a single file per project
    /// rather than the whole listing.
    private static func projectByRecency(_ root: URL, now: Date) -> [(at: Date, session: URL)] {
        let manager = FileManager.default
        guard let project = try? manager.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: [.skipsHiddenFiles]
        ) else { return [] }

        var found: [(at: Date, session: URL)] = []

        for directory in project {
            guard let modified = try? directory.resourceValues(
                forKeys: [.contentModificationDateKey]
            ).contentModificationDate else { continue }
            guard now.timeIntervalSince(modified) <= maxSessionAge else { continue }

            // The first `.jsonl` in the directory. Any one yields the project's
            // `cwd`; the newest is not needed, so this avoids stat-ing the rest.
            guard let session = try? manager.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles]
            ).first(where: { $0.pathExtension == "jsonl" }) else { continue }

            found.append((at: modified, session: session))
        }

        return found.sorted { $0.at > $1.at }
    }

    /// How many bytes of a session log to read looking for `cwd`.
    ///
    /// The field is in the first handful of lines, but a transcript can run to
    /// megabytes, and reading one whole just to reach line five — across every
    /// project directory on the machine — put seven seconds on the hotkey path.
    /// 64 KB covers far more than `cwdScanLine` lines of any real header.
    private static let cwdReadByte = 64 * 1024

    /// The `cwd` recorded in a session log's opening entries.
    ///
    /// Reads a bounded prefix, not the whole file. `String(contentsOf:)` pulls
    /// the entire transcript into memory; a take cannot wait on that.
    private static func readCwd(_ path: URL) -> URL? {
        guard let handle = try? FileHandle(forReadingFrom: path) else { return nil }
        defer { try? handle.close() }

        guard let data = try? handle.read(upToCount: cwdReadByte), !data.isEmpty,
              let text = String(data: data, encoding: .utf8)
        else { return nil }

        for line in text.split(separator: "\n", omittingEmptySubsequences: true).prefix(cwdScanLine) {
            guard let entry = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any]
            else { continue }
            // Only the top-level field. `cwd` also appears nested inside hook
            // payloads and tool results, where it may name a subdirectory the
            // session merely touched rather than the project root.
            if let cwd = entry["cwd"] as? String, !cwd.isEmpty {
                return URL(fileURLWithPath: cwd)
            }
        }

        return nil
    }

    // MARK: - Harvest

    /// The harvested bias, split where the caller must interleave its own.
    ///
    /// The ranking is the whole design, and it is not one list because the
    /// builtin developer vocabulary belongs *between* these two: a dictation is
    /// most likely to contain the name of the repo it is about, then ordinary
    /// programming words, then the names of the other repos and hosts on this
    /// machine, and only last the jargon inside the current README. Returning
    /// one flat list let a machine with two hundred catalog entries evict
    /// `TypeScript` and `OAuth`, which is a worse trade than dropping the tail
    /// of an inventory.
    struct Harvest {
        var identity: [String] = []
        var house: [String] = []
        var vocabulary: [String] = []
    }

    static func harvest(catalog: URL?) -> Harvest {
        // One walk of the project directories serves both the active project and
        // the recent list: the first directory that yields a name is the active
        // project, and the rest fill the recent list. One stat per directory,
        // one bounded file read per unique project.
        var name: [String] = []
        var seen = Set<String>()
        var project: Resolved?
        for directory in projectByRecency(sessionRoot, now: Date()) {
            guard let cwd = readCwd(directory.session) else { continue }
            let found = cwd.lastPathComponent
            guard found.count >= minTermLength, found.count <= maxNameLength else { continue }
            if project == nil { project = Resolved(root: cwd, name: found) }
            if seen.insert(found.lowercased()).inserted { name.append(found) }
            if name.count >= recentLimit { break }
        }

        var house = Term()
        for one in name { house.pushCatalogName(one) }
        for one in catalogKeyterm(catalog) { house.insert(one) }

        return Harvest(
            identity: project.map(identity) ?? [],
            house: clip(house.kept, budget: houseCharBudget),
            vocabulary: project.map(vocabulary) ?? []
        )
    }

    /// Recogniser bias harvested from one project directory, most specific
    /// first. Kept separate from `harvest` so it can be tested on a fixture.
    static func keyterm(_ project: Resolved) -> [String] {
        identity(project) + vocabulary(project)
    }

    /// What this project is called: its own name, its branch, its package. The
    /// terms a dictation about it is most certain to contain.
    private static func identity(_ project: Resolved) -> [String] {
        var term = Term()

        term.pushName(project.name)
        term.extendToken(project.name)

        if let found = branch(project.root) { term.extendToken(found) }

        for name in packageName(project.root) {
            term.pushName(name)
            term.extendToken(name)
        }

        return term.kept
    }

    /// What this project talks about: its directories, and the identifiers its
    /// README puts in backticks. Real vocabulary, but the tail of it is noise,
    /// so it ranks below anything that is a proper noun elsewhere on the
    /// machine.
    private static func vocabulary(_ project: Resolved) -> [String] {
        var term = Term()
        for name in topLevelDirectory(project.root) { term.extendToken(name) }
        for found in readmeTerm(project.root) { term.extendToken(found) }
        return term.kept
    }

    /// The active branch, read from `.git/HEAD` rather than shelled out to
    /// `git`.
    ///
    /// The extension runs `git rev-parse --abbrev-ref HEAD`, which costs a
    /// process spawn on the hotkey path and needs git on `PATH`. `.git/HEAD` is
    /// one line of plain text and says the same thing. A detached head has no
    /// branch name to harvest, which is the same case the extension skips.
    private static func branch(_ root: URL) -> String? {
        guard let head = try? String(
            contentsOf: root.appendingPathComponent(".git/HEAD"), encoding: .utf8
        ) else { return nil }

        let reference = head.trimmingCharacters(in: .whitespacesAndNewlines)
        guard reference.hasPrefix("ref: refs/heads/") else { return nil }
        let name = String(reference.dropFirst("ref: refs/heads/".count))
        return name.isEmpty ? nil : name
    }

    /// The declared package name from whichever manifest the project has.
    ///
    /// Parsed by line rather than with a TOML dependency: every one of these is
    /// a `name` key with a quoted string value, and the first such key in the
    /// file is the package's own. A workspace root without a `[package]`
    /// section yields nothing, which is correct — there is no single name.
    private static func packageName(_ root: URL) -> [String] {
        var found: [String] = []

        for manifest in ["Cargo.toml", "pyproject.toml"] {
            guard let text = try? String(
                contentsOf: root.appendingPathComponent(manifest), encoding: .utf8
            ) else { continue }

            var inPackage = false
            for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
                let line = raw.trimmingCharacters(in: .whitespaces)
                if line.hasPrefix("[") {
                    inPackage = line == "[package]" || line == "[project]"
                    continue
                }
                guard inPackage, line.hasPrefix("name") else { continue }
                if let name = quoted(String(line.dropFirst("name".count))) {
                    found.append(name)
                    break
                }
            }
        }

        if let text = try? Data(contentsOf: root.appendingPathComponent("package.json")),
           let manifest = try? JSONSerialization.jsonObject(with: text) as? [String: Any],
           let name = manifest["name"] as? String {
            // Scoped packages carry the org in the name; the bare package is
            // the half anyone says out loud.
            found.append(name.split(separator: "/").last.map(String.init) ?? name)
        }

        return found
    }

    /// The string inside the first pair of quotes, if any.
    static func quoted(_ value: String) -> String? {
        let quote: Set<Character> = ["\"", "'"]
        guard let open = value.firstIndex(where: quote.contains) else { return nil }
        let rest = value[value.index(after: open)...]
        guard let close = rest.firstIndex(where: quote.contains) else { return nil }
        let inner = String(rest[..<close])
        return inner.isEmpty ? nil : inner
    }

    /// Top-level directory names, minus the ones every project has.
    private static func topLevelDirectory(_ root: URL) -> [String] {
        let ignored: Set<String> = [
            "node_modules", "target", "build", "dist", "out", "vendor",
            "venv", "__pycache__", "coverage", "tmp", "temp", "bin",
        ]

        guard let entry = try? FileManager.default.contentsOfDirectory(
            at: root, includingPropertiesForKeys: [.isDirectoryKey], options: [.skipsHiddenFiles]
        ) else { return [] }

        var found: [String] = []
        for directory in entry {
            guard (try? directory.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true
            else { continue }
            let name = directory.lastPathComponent
            // Dotted directories are tooling, not vocabulary.
            guard !name.hasPrefix("."), !ignored.contains(name.lowercased()) else { continue }
            found.append(name)
            if found.count >= maxDirectory { break }
        }

        return found
    }

    /// Identifiers a README puts in backticks, plus the words in its headings.
    ///
    /// Backticked spans are the highest-value source in the whole harvester: a
    /// README wraps exactly the file names, commands, flags and API names that
    /// a general recogniser has never seen and a dictation about the project
    /// will say out loud.
    private static func readmeTerm(_ root: URL) -> [String] {
        guard let text = ["README.md", "README", "readme.md"].lazy.compactMap({
            try? String(contentsOf: root.appendingPathComponent($0), encoding: .utf8)
        }).first else { return [] }

        var found: [String] = []

        // Backtick spans. Fences open and close with the same character, so a
        // fenced block reads as one enormous "span" — the bounds discard it.
        for (index, chunk) in text.components(separatedBy: "`").enumerated()
        where index % 2 == 1 && chunk.count <= 120 && !chunk.contains("\n") {
            found.append(chunk)
        }

        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = raw.drop(while: { $0 == " " })
            guard line.hasPrefix("#") else { continue }
            found.append(String(line.drop(while: { $0 == "#" || $0 == " " })))
        }

        return Array(found.prefix(maxReadmeTerm))
    }

    // MARK: - Catalog

    /// Well-known catalog locations, probed when the setting names none.
    ///
    /// Only `booted` so far — it caches its scan to a plain JSON file, which is
    /// why this reads a file rather than the `:5173` endpoint that serves the
    /// same data. A file read cannot hang on the hotkey path, and splaude has
    /// no business shipping an HTTP client that talks to whatever a setting
    /// points at.
    private static var knownCatalog: [URL] {
        [FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".booted/inventory.json")]
    }

    /// Names harvested from a JSON catalog of the machine's infrastructure.
    ///
    /// Ordered as the file is: an inventory is usually written
    /// newest-or-most-used first, and truncation at the wire budget makes that
    /// ordering matter.
    static func catalogKeyterm(_ path: URL?) -> [String] {
        for candidate in path.map({ [$0] }) ?? knownCatalog {
            let size = (try? FileManager.default.attributesOfItem(
                atPath: candidate.path
            )[.size] as? Int) ?? nil
            if let size, size > catalogByteLimit {
                Diagnostic.log("project", "catalog too large: \(candidate.path)")
                continue
            }

            guard let data = try? Data(contentsOf: candidate) else { continue }
            guard let value = try? JSONSerialization.jsonObject(with: data) else {
                Diagnostic.log("project", "catalog is not JSON: \(candidate.path)")
                continue
            }

            var term = Term()
            harvestName(value, into: &term)
            Diagnostic.log("project", "catalog \(candidate.path) — \(term.kept.count) name")
            return term.kept
        }

        return []
    }

    /// Walks any JSON shape, keeping the string values under `catalogNameKey`.
    private static func harvestName(_ value: Any, into term: inout Term) {
        if let field = value as? [String: Any] {
            // An object with a `pid` is a running process, and its `name` is an
            // OS process name — `node`, `rapportd`, `ControlCe` truncated by the
            // kernel. None of that is a word anyone dictates, and on a busy
            // machine there is more of it than there are repos.
            let isProcess = field["pid"] != nil

            for (key, inner) in field {
                if isProcess && key == "name" { continue }
                if catalogNameKey.contains(key), let found = inner as? String,
                   !found.contains(" ") {
                    // A name is one word or a short hyphenated one. A sentence
                    // under a `name` key is a description.
                    term.pushCatalogName(found)
                }
                harvestName(inner, into: &term)
            }
        } else if let item = value as? [Any] {
            for inner in item { harvestName(inner, into: &term) }
        }
    }

    // MARK: - Term list

    /// An ordered, deduped term list. Deduplication is case-insensitive so
    /// `splaude` and `Splaude` do not both spend budget.
    struct Term {
        private var seen = Set<String>()
        private(set) var kept: [String] = []

        mutating func insert(_ value: String) {
            if seen.insert(value.lowercased()).inserted { kept.append(value) }
        }

        /// A whole name, kept as-is and allowed the longer bound.
        mutating func pushName(_ raw: String) {
            let value = raw
                .trimmingCharacters(in: .whitespaces)
                .trimmingCharacters(in: CharacterSet(charactersIn: "-_."))
            guard value.count >= minTermLength, value.count <= maxNameLength else { return }
            guard !digitHeavy(value), !stopWord.contains(value.lowercased()) else { return }
            insert(value)
        }

        /// A catalog name: the whole thing, plus its leading segment.
        ///
        /// Deliberately *not* the full token split the rest of the harvester
        /// uses. An inventory is mostly `{project}-{role}` pairs, and splitting
        /// them yields `api`, `web`, `app`, `auth`, `site` — words a recogniser
        /// already knows perfectly and which spend budget that `fourlinq` and
        /// `bygelo` needed. The leading segment is the project, which is the
        /// half worth having on its own.
        mutating func pushCatalogName(_ value: String) {
            pushName(value)
            if let lead = value.split(whereSeparator: { $0 == "-" || $0 == "_" || $0 == "." }).first,
               lead.count < value.count {
                pushName(String(lead))
            }
        }

        /// Splits an identifier into the words someone would say, and keeps
        /// each. `rust-workspace` is two spoken words, and so is
        /// `speechBackend`; the recogniser is helped by the parts, not by the
        /// joined form it will never hear.
        mutating func extendToken(_ value: String) {
            for word in splitIdentifier(value) {
                guard word.count >= minTermLength, word.count <= maxTermLength else { continue }
                // A README is full of version numbers, ports and hex colours.
                // Nobody dictates `E8763` or `600`, and a recogniser biased
                // toward them will hear them in noise. A digit inside a word is
                // fine — `nova3` and `linear16` are said out loud — so this is
                // a ratio and not a ban.
                guard !digitHeavy(word), !stopWord.contains(word.lowercased()) else { continue }
                insert(word)
            }
        }
    }

    /// Whether a token is mostly digits, and so an identifier nobody
    /// pronounces.
    static func digitHeavy(_ value: String) -> Bool {
        let total = value.count
        let digit = value.filter(\.isNumber).count
        return total == 0 || digit * 5 >= total * 2
    }

    /// `camelCase`, `PascalCase`, `snake_case`, `kebab-case` and paths, all
    /// split into their words. An acronym run stays whole: `HTTPServer` is
    /// `HTTP` and `Server`, not `H`, `T`, `T`, `P` and `Server`.
    static func splitIdentifier(_ value: String) -> [String] {
        var word: [String] = []
        var current = ""

        let character = Array(value)
        for (index, this) in character.enumerated() {
            guard this.isLetter || this.isNumber else {
                if !current.isEmpty { word.append(current); current = "" }
                continue
            }

            let previous = index > 0 ? character[index - 1] : nil
            let next = index + 1 < character.count ? character[index + 1] : nil

            // A capital starts a new word when it follows a lowercase letter
            // (`fooBar`) or begins one inside an acronym run (`HTTPServer`).
            var boundary = false
            if this.isUppercase, let previous {
                if previous.isLowercase || previous.isNumber {
                    boundary = true
                } else if let next, previous.isUppercase, next.isLowercase {
                    boundary = true
                }
            }

            if boundary, !current.isEmpty { word.append(current); current = "" }
            current.append(this)
        }

        if !current.isEmpty { word.append(current) }

        // The joined form is worth keeping only when it was a single word to
        // begin with — otherwise it is a spelling no one pronounces.
        if word.count > 1, value.allSatisfy({ $0.isLetter || $0.isNumber }) {
            word.append(value)
        }

        return word
    }

    /// Keeps the leading terms that fit inside `budget` characters, commas
    /// counted the way the packer counts them.
    static func clip(_ term: [String], budget: Int) -> [String] {
        var length = 0
        var kept: [String] = []

        for found in term {
            let cost = found.count + (kept.isEmpty ? 0 : 1)
            guard length + cost <= budget else { break }
            length += cost
            kept.append(found)
        }

        return kept
    }

    // MARK: - Cache

    /// The active project's bias, recomputed at most once every `cacheTTL`.
    ///
    /// A take starts on a hotkey press and the socket opens immediately, so the
    /// harvest sits on the latency path. Reading a README and a directory
    /// listing is sub-millisecond, but doing it on every press for a repo that
    /// has not changed is waste — and the answer only moves when the user
    /// switches project or branch, which is not a per-take event.
    // Five minutes, not thirty seconds: a cold refresh stats every project
    // directory on the machine, and while the OS keeps that warm after the
    // first touch, memory pressure can evict it — so a short TTL risks paying
    // the cold cost again mid-session. Switching repo or branch more than once
    // in five minutes is rare, and a relaunch always re-warms immediately.
    private static let cacheTTL: TimeInterval = 300
    private static let cacheLock = NSLock()
    private static var cache: (at: Date, harvest: Harvest)?
    private static var refreshing = false
    private static let refreshQueue = DispatchQueue(label: "com.bygelo.splaude.harvest")

    /// The last harvest, refreshed in the background — never computed inline.
    ///
    /// This is read at the very start of a take, on the hotkey path, so it must
    /// not touch the filesystem: the harvest walks every project directory on
    /// the machine, and doing that synchronously put whole seconds between the
    /// keypress and the microphone. Instead it returns whatever is cached right
    /// now — possibly stale, possibly empty on the first ever take — and kicks
    /// off a background refresh when the value is missing or old. The next take
    /// gets the fresh list; this one never waits.
    static func cachedHarvest(catalog: URL?) -> Harvest {
        cacheLock.lock()
        let current = cache
        let stale = current.map { Date().timeIntervalSince($0.at) >= cacheTTL } ?? true
        let shouldRefresh = stale && !refreshing
        if shouldRefresh { refreshing = true }
        cacheLock.unlock()

        if shouldRefresh {
            refreshQueue.async {
                let found = harvest(catalog: catalog)
                Diagnostic.log(
                    "project",
                    "harvest — \(found.identity.count) identity, "
                        + "\(found.house.count) house, \(found.vocabulary.count) vocabulary"
                )
                cacheLock.lock()
                cache = (at: Date(), harvest: found)
                refreshing = false
                cacheLock.unlock()
            }
        }

        return current?.harvest ?? Harvest()
    }

    /// Warm the cache before the first take, so even that one carries the bias.
    ///
    /// Called once at launch. Without it the first dictation after opening the
    /// app ships no project terms, because the background refresh it triggers
    /// has not finished yet.
    static func warm(catalog: URL?) {
        _ = cachedHarvest(catalog: catalog)
    }
}
