import XCTest

@testable import splaude

/// The harvester is the accuracy feature, and every filter in it is a decision
/// about what does *not* get to spend the 1024-character wire budget. A rule
/// that silently changes is a term the user stops getting for no visible reason.
///
/// Mirrors `Crate/core/src/project.rs`'s own tests; the two ports must agree.
final class ProjectTest: XCTestCase {

    private var scratch: URL!

    override func setUpWithError() throws {
        scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("splaude-project-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: scratch)
    }

    // MARK: - Splitting

    func testSplitsAKebabIdentifier() {
        XCTAssertEqual(Project.splitIdentifier("rust-workspace"), ["rust", "workspace"])
    }

    func testSplitsCamelCaseAndKeepsTheJoinedForm() {
        XCTAssertEqual(
            Project.splitIdentifier("speechBackend"),
            ["speech", "Backend", "speechBackend"]
        )
    }

    func testKeepsAnAcronymRunWhole() {
        // The failure this guards is `HTTPServer` arriving as five terms of one
        // letter each, which is budget spent on nothing.
        XCTAssertEqual(Project.splitIdentifier("HTTPServer"), ["HTTP", "Server", "HTTPServer"])
    }

    func testSplitsAPath() {
        XCTAssertEqual(Project.splitIdentifier("Crate/core/src"), ["Crate", "core", "src"])
    }

    func testDoesNotInventAJoinedFormForASeparatedIdentifier() {
        // `rust-workspace` is never said as one word, so shipping it as a term
        // would only teach the recogniser a spelling it will never hear.
        XCTAssertFalse(Project.splitIdentifier("rust-workspace").contains("rust-workspace"))
    }

    // MARK: - Filters

    func testDigitHeavyDropsANumberButKeepsAPronouncedIdentifier() {
        XCTAssertTrue(Project.digitHeavy("600"))
        XCTAssertTrue(Project.digitHeavy("E8763"))
        XCTAssertFalse(Project.digitHeavy("nova3"))
        XCTAssertFalse(Project.digitHeavy("linear16"))
        XCTAssertFalse(Project.digitHeavy("splaude"))
    }

    func testTermDedupesCaseInsensitively() {
        var term = Project.Term()
        term.pushName("splaude")
        term.pushName("Splaude")
        XCTAssertEqual(term.kept, ["splaude"])
    }

    func testTermDropsAWordTooShortToBeWorthBiasing() {
        var term = Project.Term()
        term.extendToken("go-to-ui")
        XCTAssertTrue(term.kept.isEmpty)
    }

    func testAStopWordNeverSpendsBudget() {
        var term = Project.Term()
        term.extendToken("src/lib/env")
        XCTAssertTrue(term.kept.isEmpty)
    }

    func testQuotedReadsTheFirstString() {
        XCTAssertEqual(Project.quoted(#" = "splaude-core""#), "splaude-core")
    }

    func testClipStopsAtTheBudgetCountingSeparators() {
        // "aaaa,bbbb" is 9 characters; one more term would be 14.
        XCTAssertEqual(Project.clip(["aaaa", "bbbb", "cccc"], budget: 9), ["aaaa", "bbbb"])
    }

    // MARK: - Harvest

    func testHarvestsARealProjectTree() throws {
        let root = scratch.appendingPathComponent("splaude")
        let manager = FileManager.default
        try manager.createDirectory(
            at: root.appendingPathComponent(".git"), withIntermediateDirectories: true)
        try manager.createDirectory(
            at: root.appendingPathComponent("Source"), withIntermediateDirectories: true)
        try manager.createDirectory(
            at: root.appendingPathComponent("node_modules"), withIntermediateDirectories: true)
        try "ref: refs/heads/rust-workspace\n"
            .write(to: root.appendingPathComponent(".git/HEAD"), atomically: true, encoding: .utf8)
        try "[workspace]\nmember = []\n\n[package]\nname = \"splaude-core\"\n"
            .write(to: root.appendingPathComponent("Cargo.toml"), atomically: true, encoding: .utf8)
        try "# splaude\n\nUse `pack_keyterm` first.\n"
            .write(to: root.appendingPathComponent("README.md"), atomically: true, encoding: .utf8)

        let term = Project.keyterm(Project.Resolved(root: root, name: "splaude"))

        XCTAssertEqual(term.first, "splaude")
        for expected in ["rust", "workspace", "splaude-core", "core", "Source", "pack", "keyterm"] {
            XCTAssertTrue(term.contains(expected), "missing \(expected): \(term)")
        }
        // Ignored directories must not spend budget.
        XCTAssertFalse(term.contains("node_modules"))
    }

    // MARK: - Session resolution

    func testResolvesTheNewestSessionToItsRecordedCwd() throws {
        let project = scratch.appendingPathComponent("-Users-someone-Antigravity-splaude")
        try FileManager.default.createDirectory(at: project, withIntermediateDirectories: true)
        // The directory name is deliberately not invertible back to the path —
        // this asserts the `cwd` field is what gets read.
        try "{\"type\":\"mode\"}\n{\"cwd\":\"/Users/someone/Antigravity/sisia_app\"}\n"
            .write(to: project.appendingPathComponent("a.jsonl"), atomically: true, encoding: .utf8)

        let found = try XCTUnwrap(Project.activeWithin(scratch, now: Date()))
        XCTAssertEqual(found.name, "sisia_app")
        XCTAssertEqual(found.root.path, "/Users/someone/Antigravity/sisia_app")
    }

    func testIgnoresASessionOlderThanTheStalenessBound() throws {
        let project = scratch.appendingPathComponent("-old")
        try FileManager.default.createDirectory(at: project, withIntermediateDirectories: true)
        try "{\"cwd\":\"/tmp/old\"}\n"
            .write(to: project.appendingPathComponent("a.jsonl"), atomically: true, encoding: .utf8)

        let later = Date().addingTimeInterval(8 * 24 * 60 * 60)
        XCTAssertNil(Project.activeWithin(scratch, now: later))
    }

    func testRecentNameIsOrderedNewestFirstAndDeduped() throws {
        // Two session directories for the same repo plus one for another; the
        // repo must appear once, at the rank of its newest session.
        for (directory, cwd) in [("-a", "/x/blead"), ("-b", "/x/booted"), ("-c", "/x/blead")] {
            let project = scratch.appendingPathComponent(directory)
            try FileManager.default.createDirectory(at: project, withIntermediateDirectories: true)
            try "{\"cwd\":\"\(cwd)\"}\n"
                .write(
                    to: project.appendingPathComponent("s.jsonl"), atomically: true, encoding: .utf8)
            // mtime ordering is what ranks these, and the filesystem's
            // resolution is coarse enough that same-instant writes tie.
            Thread.sleep(forTimeInterval: 0.02)
        }

        XCTAssertEqual(Project.recentNameWithin(scratch, now: Date(), limit: 10), ["blead", "booted"])
    }

    func testRecentNameHonoursTheLimit() throws {
        for index in 0..<5 {
            let project = scratch.appendingPathComponent("-\(index)")
            try FileManager.default.createDirectory(at: project, withIntermediateDirectories: true)
            try "{\"cwd\":\"/x/repo\(index)name\"}\n"
                .write(
                    to: project.appendingPathComponent("s.jsonl"), atomically: true, encoding: .utf8)
        }
        XCTAssertEqual(Project.recentNameWithin(scratch, now: Date(), limit: 2).count, 2)
    }

    // MARK: - Catalog

    func testCatalogHarvestsNamesAndSkipsRunningProcesses() throws {
        let path = scratch.appendingPathComponent("inventory.json")
        try """
        {
          "repo": [{"name": "fourlinq-hr"}, {"name": "bygelo"}],
          "vps": [{"host": "advo"}],
          "process": [{"pid": 42, "name": "rapportd", "port": 5432}],
          "note": {"name": "a whole sentence under a name key"}
        }
        """.write(to: path, atomically: true, encoding: .utf8)

        let found = Project.catalogKeyterm(path)

        XCTAssertTrue(found.contains("fourlinq-hr"))
        // The leading segment ships too — `fourlinq` is said on its own.
        XCTAssertTrue(found.contains("fourlinq"))
        XCTAssertTrue(found.contains("bygelo"))
        XCTAssertTrue(found.contains("advo"))
        // A process name is an OS artefact, not a word anyone dictates.
        XCTAssertFalse(found.contains("rapportd"))
        // `hr` never ships alone: catalog names are not token-split, which is
        // what keeps `api`, `web` and `auth` out of the budget.
        XCTAssertFalse(found.contains("hr"))
        XCTAssertFalse(found.contains(where: { $0.contains(" ") }))
    }

    func testCatalogIsSilentAboutAFileThatIsNotJSON() throws {
        let path = scratch.appendingPathComponent("bad.json")
        try "not json".write(to: path, atomically: true, encoding: .utf8)
        XCTAssertTrue(Project.catalogKeyterm(path).isEmpty)
    }
}
