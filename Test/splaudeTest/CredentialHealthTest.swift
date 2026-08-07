import XCTest

@testable import splaude

/// splaude reads the Claude Code token but never refreshes it, so classifying
/// how close it is to death is what stands between the user and a dictation
/// that fails for no visible reason. `classify` takes `now` precisely so these
/// thresholds can be exercised against crafted credentials rather than
/// whatever the Keychain happens to hold.
final class CredentialHealthTest: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_800_000_000)
    private let minute: TimeInterval = 60

    private func credential(expiringIn offset: TimeInterval?) -> TokenStore.Credential {
        TokenStore.Credential(
            accessToken: "sk-test",
            expiresAt: offset.map { (now.timeIntervalSince1970 + $0) * 1000 },
            source: "test"
        )
    }

    func testACredentialWithNoStatedExpiryIsUsable() {
        guard case .usable(let until) = TokenStore.classify(credential(expiringIn: nil), now: now)
        else {
            return XCTFail("expected .usable")
        }
        XCTAssertNil(until)
    }

    func testAPastExpiryIsExpired() {
        guard case .expired = TokenStore.classify(credential(expiringIn: -minute), now: now) else {
            return XCTFail("expected .expired")
        }
    }

    func testExpiringExactlyNowCountsAsExpired() {
        guard case .expired = TokenStore.classify(credential(expiringIn: 0), now: now) else {
            return XCTFail("expected .expired")
        }
    }

    func testInsideTheWarnWindowIsExpiringSoon() {
        guard case .expiringSoon = TokenStore.classify(credential(expiringIn: 5 * minute), now: now)
        else {
            return XCTFail("expected .expiringSoon")
        }
    }

    func testTheWarnWindowIsTenMinutes() {
        // Just inside warns; comfortably outside does not. This boundary is
        // what decides whether the menu nags before a take or after it fails.
        guard case .expiringSoon = TokenStore.classify(credential(expiringIn: 10 * minute), now: now)
        else {
            return XCTFail("10 minutes should warn")
        }
        guard case .usable = TokenStore.classify(credential(expiringIn: 11 * minute), now: now)
        else {
            return XCTFail("11 minutes should not warn")
        }
    }

    func testOnlyAUsableCredentialNeedsNoAttention() {
        XCTAssertFalse(TokenStore.Health.usable(until: nil).needsAttention)
        XCTAssertTrue(TokenStore.Health.expired.needsAttention)
        XCTAssertTrue(TokenStore.Health.expiringSoon(now).needsAttention)
        XCTAssertTrue(TokenStore.Health.missing("gone").needsAttention)
    }

    func testOnlyAUsableCredentialHasNoHeadline() {
        XCTAssertNil(TokenStore.Health.usable(until: nil).headline)
        XCTAssertNotNil(TokenStore.Health.expired.headline)
        XCTAssertNotNil(TokenStore.Health.missing("gone").headline)
    }

    func testIsExpiredTracksTheStatedExpiry() {
        XCTAssertFalse(credential(expiringIn: nil).isExpired)
        XCTAssertTrue(
            TokenStore.Credential(accessToken: "sk", expiresAt: 1000, source: "test").isExpired
        )
    }
}
