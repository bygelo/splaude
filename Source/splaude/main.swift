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

    // The whole point of project bias is that the user never configures it, so
    // this is the only place they can see what it decided. A wrong project or a
    // wrong term is otherwise invisible until a dictation comes back mangled.
    print("\nrecogniser bias")
    switch (Project.active(), Setting.useProjectKeyterm) {
    case (let project?, true):
        print("  project   \(project.name) (\(project.root.path))")
    case (let project?, false):
        print("  project   \(project.name) — off in the setting")
    case (nil, _):
        print("  project   none — no recent Claude Code session")
    }
    let catalogPath = Setting.catalogPath
    let catalog = Project.catalogKeyterm(catalogPath.isEmpty ? nil : URL(fileURLWithPath: catalogPath))
    print("  catalog   \(catalog.isEmpty ? "none found" : "\(catalog.count) name")")
    print("  recent    \(Project.recentName(8).joined(separator: ", "))")
    let packed = AnthropicSpeechBackend.packKeyterm(Setting.wireKeytermSync)
    print("  budget    \(packed.count) of 1024 characters")
    print("  keyterm   \(packed)")

    exit(0)
}

// `splaude --bench "phrase"` records once and replays the same audio with and
// without the keyterm bias, which is the only way to measure the bias rather
// than measuring how differently you said the phrase the second time.
if let index = CommandLine.arguments.firstIndex(of: "--bench") {
    Benchmark.run(Array(CommandLine.arguments[(index + 1)...]))
}

let application = NSApplication.shared
let delegate = AppDelegate()
application.delegate = delegate
application.setActivationPolicy(.accessory)
application.run()
