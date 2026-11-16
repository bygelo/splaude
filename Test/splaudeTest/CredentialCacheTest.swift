import XCTest

@testable import splaude

/// The Keychain item holding the token belongs to Claude Code, so reading it is
/// an ACL decision macOS may answer with a password dialog — once per launch of
/// a rebuilt app, and once per token refresh. splaude keeps its own copy so the
/// dialog is asked for a credential rather than for a launch; these cover the
/// path that must never touch the Keychain at all.
final class CredentialCacheTest: XCTestCase {

    private var cache: URL!

    override func setUp() {
        super.setUp()
        cache = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("splaude-credential-test-\(UUID().uuidString).json")
        // Before invalidate(), which deletes whatever cacheFile points at — the
        // real one included.
        TokenStore.cacheFile = cache
        TokenStore.invalidate()
    }

    override func tearDown() {
        try? FileManager.default.removeItem(at: cache)
        TokenStore.invalidate()
        super.tearDown()
    }

    private func write(expiringIn offset: TimeInterval) {
        let expiresAt = (Date().timeIntervalSince1970 + offset) * 1000
        let payload = #"{"accessToken":"sk-cached","expiresAt":\#(expiresAt)}"#
        try! payload.write(to: cache, atomically: true, encoding: .utf8)
    }

    func testALiveCachedTokenIsServedWithoutTheKeychain() {
        write(expiringIn: 3600)

        guard let credential = TokenStore.unpromptedCredential() else {
            return XCTFail("expected the cached token")
        }
        XCTAssertEqual(credential.accessToken, "sk-cached")
    }

    func testAnExpiredCachedTokenIsNotServed() {
        write(expiringIn: -60)
        XCTAssertNil(TokenStore.unpromptedCredential())
    }

    func testNoCacheMeansNoUnpromptedToken() {
        XCTAssertNil(TokenStore.unpromptedCredential())
    }

    /// A token the server has already refused must not come back from disk on
    /// the next take, or every retry reuses it.
    func testInvalidateRemovesTheCachedCopy() {
        write(expiringIn: 3600)
        XCTAssertNotNil(TokenStore.unpromptedCredential())

        TokenStore.invalidate()

        XCTAssertFalse(FileManager.default.fileExists(atPath: cache.path))
        XCTAssertNil(TokenStore.unpromptedCredential())
    }
}
