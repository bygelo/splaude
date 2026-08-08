import XCTest

@testable import splaude

/// Keyterm packing is the single highest-leverage accuracy setting, and it is
/// the one place the app rewrites the user's input before putting it on the
/// wire. A term silently dropped here is a word the recogniser keeps mangling
/// with no indication why.
final class KeytermTest: XCTestCase {

    private let budget = 1024

    func testJoinsTermWithCommas() {
        XCTAssertEqual(
            AnthropicSpeechBackend.packKeyterm(["grep", "regex"]),
            "grep,regex"
        )
    }

    func testDropsADuplicate() {
        XCTAssertEqual(
            AnthropicSpeechBackend.packKeyterm(["grep", "grep", "regex"]),
            "grep,regex"
        )
    }

    func testStripsACommaInsideATerm() {
        // A comma would otherwise split one term into two on the wire.
        XCTAssertEqual(AnthropicSpeechBackend.packKeyterm(["one,two"]), "one two")
    }

    func testStripsNonASCIIAndCollapsesWhitespace() {
        XCTAssertEqual(
            AnthropicSpeechBackend.packKeyterm(["  héllo   wörld  "]),
            "hllo wrld"
        )
    }

    func testSkipsATermThatIsEmptyAfterCleaning() {
        XCTAssertEqual(
            AnthropicSpeechBackend.packKeyterm(["   ", "日本語", "kept"]),
            "kept"
        )
    }

    func testAnEmptyListPacksToNothing() {
        XCTAssertEqual(AnthropicSpeechBackend.packKeyterm([]), "")
    }

    func testStopsAtTheBudgetRatherThanTruncatingATerm() {
        let long = String(repeating: "x", count: 600)
        let packed = AnthropicSpeechBackend.packKeyterm([
            long, String(repeating: "y", count: 600), "zzz",
        ])
        // Two 600-character terms plus a separator exceed the budget, so the
        // second is dropped whole and the loop stops rather than backfilling.
        XCTAssertEqual(packed, long)
        XCTAssertLessThanOrEqual(packed.count, budget)
    }

    func testTheBuiltinListFitsInsideTheBudget() {
        // If the shipped list ever outgrows the budget, terms would silently
        // vanish from the wire.
        let packed = AnthropicSpeechBackend.packKeyterm(Setting.builtinKeyterm)
        XCTAssertLessThanOrEqual(packed.count, budget)
        XCTAssertTrue(packed.contains("IntelliSense"))
        XCTAssertTrue(packed.contains("worktree"))
    }
}
