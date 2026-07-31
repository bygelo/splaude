import Foundation

/// Records what the speech connection's handshake says about rate limiting.
///
/// Anthropic's Claude-metered endpoints answer with `anthropic-ratelimit-*`
/// headers describing remaining requests and tokens. If the speech socket
/// returns none of them, nothing on the Claude meter was touched — which is the
/// closest thing to proof available from the client side, short of watching the
/// account's usage page across a long dictation.
enum QuotaWatch {

    private(set) static var lastHeader: [String: String] = [:]
    private(set) static var sawRateLimitHeader = false

    /// Header names that would indicate this request counted against something.
    private static let interesting = [
        "anthropic-ratelimit", "x-ratelimit", "ratelimit",
        "anthropic-organization", "retry-after", "x-should-retry",
    ]

    static func record(_ response: HTTPURLResponse) {
        var captured: [String: String] = [:]

        for (rawKey, rawValue) in response.allHeaderFields {
            guard let key = rawKey as? String, let value = rawValue as? String else { continue }
            let lower = key.lowercased()
            guard interesting.contains(where: { lower.hasPrefix($0) }) else { continue }
            captured[lower] = value
        }

        lastHeader = captured
        sawRateLimitHeader = captured.keys.contains { $0.contains("ratelimit") }

        Diagnostic.log("quota", "handshake HTTP \(response.statusCode)")

        if captured.isEmpty {
            Diagnostic.log("quota", "no rate-limit headers — nothing metered on this connection")
        } else {
            for (key, value) in captured.sorted(by: { $0.key < $1.key }) {
                Diagnostic.log("quota", "\(key): \(value)")
            }
        }

        // Anything unexpected is worth seeing in full, once, rather than being
        // silently filtered out by the list above.
        let all = response.allHeaderFields.compactMap { key, _ in (key as? String)?.lowercased() }
        Diagnostic.log("quota", "all headers: \(all.sorted().joined(separator: ", "))")
    }

    /// One-line answer for the Settings window.
    static var summary: String {
        guard !lastHeader.isEmpty || sawRateLimitHeader else {
            return lastHeaderSeen ? "none seen" : "dictate once to check"
        }
        return sawRateLimitHeader
            ? lastHeader.filter { $0.key.contains("ratelimit") }
                .map { "\($0.key)=\($0.value)" }
                .sorted()
                .joined(separator: ", ")
            : "none seen"
    }

    private static var lastHeaderSeen = false

    static func markConnected() { lastHeaderSeen = true }
}
