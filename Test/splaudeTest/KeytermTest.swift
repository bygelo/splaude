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

/// The transport, separately from the packing.
///
/// splaude briefly sent keyterms as `keyterms` query parameters as well as in
/// the `x-config-keyterms` header, because the 2.1.98 extension bundle sends
/// them that way. Measured against the live endpoint, a take carrying those
/// parameters is answered with `TranscriptError` and a dropped socket — with or
/// without the header alongside — while the header alone transcribes and
/// measurably biases the result. These lock the header contract and the term
/// ceiling that the same measurement found.
final class KeytermTransportTest: XCTestCase {

    func testNormaliseReturnsTheTermsThePackerJoins() {
        let term = ["one,two", "grep", "grep", "  h\u{e9}llo w\u{f6}rld  "]
        XCTAssertEqual(
            AnthropicSpeechBackend.normaliseKeyterm(term).joined(separator: ","),
            AnthropicSpeechBackend.packKeyterm(term)
        )
    }

    func testStopsAtTheServersTermCeiling() {
        // 93 terms transcribe and 94 fail outright, so a harvest that grew past
        // the ceiling would not degrade the bias — it would produce no text at
        // all. This is the guard against that regression.
        let term = (0..<400).map { "term\($0)aaa" }
        let kept = AnthropicSpeechBackend.normaliseKeyterm(term)
        XCTAssertLessThanOrEqual(kept.count, 64)
        XCTAssertEqual(kept.count, 64, "the ceiling, not the byte budget, should bind here")
    }

    func testStillStopsAtTheByteBudgetWhenTermsAreLong() {
        let term = (0..<64).map { String(repeating: "a", count: 60) + "\($0)" }
        XCTAssertLessThanOrEqual(AnthropicSpeechBackend.packKeyterm(term).count, 1024)
    }
}
