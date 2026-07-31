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

    /// Resolves a usable token, preferring the Keychain.
    static func load() throws -> Credential {
        var firstFailure: Error?

        for attempt in [readKeychain, readFile] {
            do {
                if let credential = try attempt() { return credential }
            } catch {
                firstFailure = firstFailure ?? error
            }
        }

        throw firstFailure ?? TokenError.notFound
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
