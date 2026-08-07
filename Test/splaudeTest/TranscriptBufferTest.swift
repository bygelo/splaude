import XCTest

@testable import splaude

/// The committed/interim bookkeeping that decides what text a take produces.
///
/// Getting this wrong does not crash — it silently drops or duplicates the
/// user's words, which is the failure mode hardest to notice and hardest to
/// reconstruct afterwards.
final class TranscriptBufferTest: XCTestCase {

    func testAnInterimDoesNotCommit() {
        var buffer = TranscriptBuffer()
        XCTAssertEqual(buffer.apply("hello", isFinal: false), "hello")
        XCTAssertEqual(buffer.committed, "")
    }

    func testAFinalCommitsAndAccumulatesSpaceJoined() {
        var buffer = TranscriptBuffer()
        XCTAssertEqual(buffer.apply("one", isFinal: true), "one")
        XCTAssertEqual(buffer.apply("two", isFinal: true), "one two")
        XCTAssertEqual(buffer.committed, "one two")
    }

    func testAnInterimDisplaysOnTopOfWhatIsCommitted() {
        var buffer = TranscriptBuffer()
        _ = buffer.apply("committed", isFinal: true)
        XCTAssertEqual(buffer.apply("pending", isFinal: false), "committed pending")
        // …without becoming part of it.
        XCTAssertEqual(buffer.committed, "committed")
    }

    func testARevisedInterimReplacesThePreviousOne() {
        var buffer = TranscriptBuffer()
        _ = buffer.apply("low testing", isFinal: false)
        XCTAssertEqual(buffer.apply("one two three", isFinal: false), "one two three")
    }

    func testBlankAndWhitespaceChunksAreIgnored() {
        var buffer = TranscriptBuffer()
        _ = buffer.apply("kept", isFinal: true)
        XCTAssertEqual(buffer.apply("", isFinal: true), "kept")
        XCTAssertEqual(buffer.apply("   \n ", isFinal: true), "kept")
        XCTAssertEqual(buffer.committed, "kept")
    }

    func testSurroundingWhitespaceIsTrimmedBeforeJoining() {
        var buffer = TranscriptBuffer()
        _ = buffer.apply("  one  ", isFinal: true)
        XCTAssertEqual(buffer.apply("  two  ", isFinal: true), "one two")
    }

    func testResetClearsTheCommittedText() {
        var buffer = TranscriptBuffer()
        _ = buffer.apply("gone", isFinal: true)
        buffer.reset()
        XCTAssertEqual(buffer.apply("fresh", isFinal: false), "fresh")
    }
}
