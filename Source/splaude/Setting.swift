import Carbon.HIToolbox
import Foundation
import ServiceManagement

/// User-tunable knobs. Everything is backed by UserDefaults so the Settings
/// window and `defaults write` are two views of the same state.
enum Setting {

    /// Posted whenever anything here changes, so the app can re-register the
    /// hotkey or show/hide the overlay without a relaunch.
    static let didChange = Notification.Name("com.bygelo.splaude.settingDidChange")

    private static let store = UserDefaults.standard

    private static func write(_ value: Any?, _ key: String) {
        store.set(value, forKey: key)
        NotificationCenter.default.post(name: didChange, object: nil)
    }

    // MARK: - Recognition

    /// Recogniser bias, shipped with the extension's own developer-speech list.
    static let builtinKeyterm = [
        "VS Code", "IDE", "webview", "IntelliSense", "MCP", "symlink", "grep",
        "regex", "localhost", "codebase", "TypeScript", "JSON", "OAuth",
        "webhook", "gRPC", "dotfiles", "subagent", "worktree",
    ]

    /// Terms the user added. Kept separate so the built-in list is never lost.
    static var customKeyterm: [String] {
        get { store.stringArray(forKey: "keyterm") ?? [] }
        set { write(newValue, "keyterm") }
    }

    static var useBuiltinKeyterm: Bool {
        get { store.object(forKey: "useBuiltinKeyterm") as? Bool ?? true }
        set { write(newValue, "useBuiltinKeyterm") }
    }

    /// What actually goes on the wire.
    static var keyterm: [String] {
        (useBuiltinKeyterm ? builtinKeyterm : []) + customKeyterm
    }

    static var language: String {
        get { store.string(forKey: "language") ?? "en" }
        set { write(newValue, "language") }
    }

    /// Supported by the recogniser; the wire accepts any BCP-47 tag, these are
    /// just the ones worth putting in a menu.
    static let availableLanguage: [(code: String, name: String)] = [
        ("en", "English"), ("en-GB", "English (UK)"), ("es", "Spanish"),
        ("fr", "French"), ("de", "German"), ("it", "Italian"),
        ("pt", "Portuguese"), ("nl", "Dutch"), ("hi", "Hindi"),
        ("ja", "Japanese"), ("ko", "Korean"), ("zh", "Chinese"),
        ("id", "Indonesian"), ("tl", "Tagalog"), ("multi", "Multilingual"),
    ]

    // MARK: - Output

    /// Types into the focused app as you speak, rewriting revised words.
    static var liveTyping: Bool {
        get { store.object(forKey: "liveTyping") as? Bool ?? true }
        set { write(newValue, "liveTyping") }
    }

    /// Microseconds between synthetic keystrokes. Lower is snappier; too low
    /// and Electron apps start dropping characters.
    static var typingInterval: Int {
        get {
            let stored = store.integer(forKey: "typingInterval")
            return stored > 0 ? stored : 1_200
        }
        set { write(max(200, min(8_000, newValue)), "typingInterval") }
    }

    /// Refuse to type into surfaces the accessibility API says are not text.
    static var guardFocus: Bool {
        get { store.object(forKey: "guardFocus") as? Bool ?? true }
        set { write(newValue, "guardFocus") }
    }

    // MARK: - Interface

    static var showFloatingButton: Bool {
        get { store.object(forKey: "showFloatingButton") as? Bool ?? true }
        set { write(newValue, "showFloatingButton") }
    }

    static var floatingButtonPoint: CGPoint? {
        get {
            guard let raw = store.dictionary(forKey: "floatingButtonPoint"),
                  let x = raw["x"] as? Double, let y = raw["y"] as? Double else { return nil }
            return CGPoint(x: x, y: y)
        }
        set {
            guard let newValue else { return }
            // Position changes constantly while dragging; no change notification.
            store.set(["x": newValue.x, "y": newValue.y], forKey: "floatingButtonPoint")
        }
    }

    static var playSound: Bool {
        get { store.object(forKey: "playSound") as? Bool ?? false }
        set { write(newValue, "playSound") }
    }

    // MARK: - Hotkey

    /// Presence, not truthiness: `kVK_ANSI_A` is keycode 0, so a `> 0` test
    /// would silently bounce anyone who bound ⌥A back to the default.
    static var hotkeyCode: UInt32 {
        get {
            guard let stored = store.object(forKey: "hotkeyCode") as? Int else {
                return UInt32(kVK_Space)
            }
            return UInt32(stored)
        }
        set { write(Int(newValue), "hotkeyCode") }
    }

    /// Presence again, not truthiness: zero is a legitimate value here, meaning
    /// a function key bound on its own. Treating it as unset silently forced
    /// Option back on and made bare function keys impossible.
    static var hotkeyModifier: UInt32 {
        get {
            guard let stored = store.object(forKey: "hotkeyModifier") as? Int else {
                return UInt32(optionKey)
            }
            return UInt32(stored)
        }
        set { write(Int(newValue), "hotkeyModifier") }
    }

    // MARK: - Login item

    static var launchAtLogin: Bool {
        get { SMAppService.mainApp.status == .enabled }
        set {
            do {
                if newValue {
                    try SMAppService.mainApp.register()
                } else {
                    try SMAppService.mainApp.unregister()
                }
                Diagnostic.log("setting", "launch at login \(newValue ? "on" : "off")")
            } catch {
                Diagnostic.log("setting", "launch at login failed: \(error.localizedDescription)")
            }
            NotificationCenter.default.post(name: didChange, object: nil)
        }
    }
}
