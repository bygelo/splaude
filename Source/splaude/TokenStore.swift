import Foundation

/// Reads the Claude Code OAuth credential that the CLI / IDE extension already
/// keeps on this machine. Two storage shapes exist in the wild:
///
///   1. macOS Keychain, generic password, service "Claude Code-credentials",
///      whose *password* is the same JSON blob as (2).
///   2. ~/.claude/.credentials.json
///
/// Both wrap the token as `{"claudeAiOauth": {"accessToken": ..., "expiresAt": ...}}`.
/// Older builds wrote the fields at the top level, so both layouts are accepted.
enum TokenStore {

    struct Credential {
        let accessToken: String
        /// Unix epoch milliseconds. Nil when the blob omits it.
        let expiresAt: Double?
        /// Where it came from, for the diagnostic command.
        let source: String

        var isExpired: Bool {
            guard let expiresAt else { return false }
            return Date().timeIntervalSince1970 * 1000 >= expiresAt
        }
    }

    enum TokenError: LocalizedError {
        case notFound
        case unreadable(String)
        case expired

        var errorDescription: String? {
            switch self {
            case .notFound:
                return "No Claude Code credential found. Run `claude` in a terminal and sign in, then try again."
            case .unreadable(let detail):
                return "Claude Code credential could not be parsed (\(detail))."
            case .expired:
                return "Claude Code credential has expired. Run `claude` in a terminal to refresh it."
            }
        }
    }

    private static let service = "Claude Code-credentials"
    private static let file = FileManager.default
        .homeDirectoryForCurrentUser
        .appendingPathComponent(".claude/.credentials.json")

    /// The credential lives in a Keychain item owned by Claude Code, so every
    /// read is an ACL decision and macOS prompts for the login password unless
    /// the user has clicked *Always Allow*. Reading it per take meant one
    /// prompt per dictation for anyone who clicked plain *Allow* — or whose
    /// grant was invalidated by a rebuild. Hold it for the session instead.
    private static var cached: Credential?
    private static let lock = NSLock()
    private static var lastRead: Date?

    /// How long to sit on an expired copy before going back to the Keychain.
    ///
    /// The cache is skipped once a token is past expiry so a refresh gets
    /// picked up — but without a floor here, an expired credential that nobody
    /// is refreshing means a Keychain hit, and possibly an authorization
    /// prompt, on every status check.
    private static let staleRecheck: TimeInterval = 10

    /// How close to expiry counts as worth warning about.
    private static let warnWindow: TimeInterval = 10 * 60

    /// Resolves a usable token, preferring the Keychain.
    ///
    /// Cached across calls. The cache is dropped once the token is past its
    /// stated expiry, so a refreshed credential is still picked up — at the
    /// cost of exactly one prompt at that point rather than one per take.
    static func load() throws -> Credential {
        lock.lock()
        defer { lock.unlock() }

        if let cached {
            if !cached.isExpired { return cached }
            // Expired, but re-reading on every call would hammer the Keychain.
            if let lastRead, Date().timeIntervalSince(lastRead) < staleRecheck {
                return cached
            }
        }

        lastRead = Date()
        var firstFailure: Error?

        for attempt in [readKeychain, readFile] {
            do {
                if let credential = try attempt() {
                    cached = credential
                    return credential
                }
            } catch {
                firstFailure = firstFailure ?? error
            }
        }

        throw firstFailure ?? TokenError.notFound
    }

    // MARK: - Health

    /// The credential's state, classified for display.
    ///
    /// splaude reads this token but never refreshes it — that is Claude Code's
    /// job, and it only happens while Claude Code runs. Someone who installs a
    /// dictation app and never thinks about it as a Claude Code accessory can
    /// therefore find it dead, so the state is worth surfacing before a take
    /// fails rather than at the moment the hotkey is pressed.
    enum Health {
        case usable(until: Date?)
        case expiringSoon(Date)
        case expired
        case missing(String)

        var needsAttention: Bool {
            if case .usable = self { return false }
            return true
        }

        /// One line for the menu bar. Nil when there is nothing to say.
        var headline: String? {
            switch self {
            case .usable:
                return nil
            case .expiringSoon(let date):
                return "Credential expires \(date.formatted(date: .omitted, time: .shortened)) — run `claude` to refresh"
            case .expired:
                return "Credential expired — run `claude` in a terminal"
            case .missing:
                return "No Claude Code credential — run `claude` and sign in"
            }
        }
    }

    /// The credential's state for display, WITHOUT ever prompting.
    ///
    /// Health is shown on a five-minute timer that runs whether or not the app
    /// is being used. It must never touch the Keychain: reading another app's
    /// credential item is an ACL decision, and macOS puts up a login-password
    /// prompt for it whenever the "Always Allow" grant does not apply — which is
    /// exactly what happens the moment the cached token passes expiry and Claude
    /// Code has rewritten the item with a fresh one. Poking the Keychain from a
    /// background timer therefore made the password dialog appear on its own,
    /// with no dictation in sight, every five minutes around each token refresh.
    ///
    /// So this classifies whatever is already cached and never reads. The token
    /// is cached at launch and refreshed lazily by an actual take (see `load`),
    /// which is the only place a Keychain read — and its possible prompt — is
    /// justified, because only a take truly needs a live token.
    static func health() -> Health {
        lock.lock()
        let known = cached
        lock.unlock()

        guard let known else {
            // Nothing cached yet — the one read that has to happen. At launch
            // this is the read the app would do anyway; there is no extra
            // prompt beyond the one first-run grant.
            do {
                return classify(try load())
            } catch {
                return .missing(error.localizedDescription)
            }
        }
        return classify(known)
    }

    /// Split from `health()` so the thresholds can be exercised against crafted
    /// credentials rather than only whatever the Keychain happens to hold.
    static func classify(_ credential: Credential, now: Date = Date()) -> Health {
        guard let raw = credential.expiresAt else { return .usable(until: nil) }

        let expiry = Date(timeIntervalSince1970: raw / 1000)
        if expiry <= now { return .expired }
        if expiry.timeIntervalSince(now) <= warnWindow { return .expiringSoon(expiry) }
        return .usable(until: expiry)
    }

    /// Drops the cached copy so the next `load()` goes back to the Keychain.
    /// Call when the server rejects the token — an expiry we were not told
    /// about looks exactly like a valid cached credential from here.
    static func invalidate() {
        lock.lock()
        defer { lock.unlock() }
        cached = nil
    }

    // MARK: - Source

    private static func readKeychain() throws -> Credential? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]

        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)

        guard status != errSecItemNotFound else { return nil }
        guard status == errSecSuccess, let data = item as? Data else {
            throw TokenError.unreadable("Keychain status \(status)")
        }

        return try parse(data, source: "Keychain (\(service))")
    }

    private static func readFile() throws -> Credential? {
        guard FileManager.default.fileExists(atPath: file.path) else { return nil }
        let data = try Data(contentsOf: file)
        return try parse(data, source: file.path)
    }

    // MARK: - Parsing

    private static func parse(_ data: Data, source: String) throws -> Credential {
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw TokenError.unreadable("not JSON — from \(source)")
        }

        // Current layout nests under claudeAiOauth; older ones are flat.
        let scope = (root["claudeAiOauth"] as? [String: Any]) ?? root

        guard let token = scope["accessToken"] as? String, !token.isEmpty else {
            throw TokenError.unreadable("no accessToken — from \(source)")
        }

        return Credential(
            accessToken: token,
            expiresAt: scope["expiresAt"] as? Double,
            source: source
        )
    }

    /// Prints what was found without ever revealing the secret. Used by `--check`.
    static func describe() -> String {
        do {
            let credential = try load()
            let fingerprint = String(credential.accessToken.prefix(12)) + "…"
            var line = "found: \(credential.source)\n  token: \(fingerprint) (\(credential.accessToken.count) chars)"
            if let expiresAt = credential.expiresAt {
                let date = Date(timeIntervalSince1970: expiresAt / 1000)
                line += "\n  expires: \(date.formatted()) — \(credential.isExpired ? "EXPIRED" : "valid")"
            } else {
                line += "\n  expires: not stated"
            }
            return line
        } catch {
            return "not found: \(error.localizedDescription)"
        }
    }
}
