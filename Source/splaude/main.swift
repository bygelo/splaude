import AppKit

// `splaude --check` verifies the credential without launching the UI, so the
// first thing to debug is never "is it even finding my token".
if CommandLine.arguments.contains("--check") {
    print("splaude credential check")
    print(TokenStore.describe())
    print("  health: \(TokenStore.health().headline ?? "usable — nothing to warn about")")
    print("\naccessibility (needed to paste): \(TextInserter.isTrusted ? "granted" : "NOT granted")")
    print("hotkey: \(Hotkey.describe())")

    print("focus: \(FocusProbe.frontmostApp) / \(FocusProbe.current().label)")
    if let anchor = FocusAnchor.capture() {
        print("anchor: \(anchor.appName) (pid \(anchor.pid)) — app active \(anchor.isAppActive), holds focus \(anchor.holdsFocus)")
    } else {
        print("anchor: none — no frontmost application")
    }
    exit(0)
}

let application = NSApplication.shared
let delegate = AppDelegate()
application.delegate = delegate
application.setActivationPolicy(.accessory)
application.run()
