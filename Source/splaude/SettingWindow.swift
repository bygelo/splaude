import AppKit
import AVFoundation
import Carbon.HIToolbox
import SwiftUI

/// Hosts the settings window. Kept as a single retained controller so reopening
/// returns to the same window rather than stacking copies.
final class SettingWindow {

    static let shared = SettingWindow()
    private var window: NSWindow?

    func show() {
        if let window {
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
            return
        }

        let controller = NSHostingController(rootView: SettingView())
        let window = NSWindow(contentViewController: controller)
        window.title = "splaude Settings"
        window.styleMask = [.titled, .closable, .miniaturizable]
        window.setContentSize(NSSize(width: 460, height: 620))
        window.isReleasedWhenClosed = false
        window.center()

        self.window = window

        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }
}

/// Mirrors Setting so SwiftUI can bind to it. Every setter writes straight
/// through, which is what posts the change notification the app listens on.
private final class SettingModel: ObservableObject {

    @Published var liveTyping = Setting.liveTyping { didSet { Setting.liveTyping = liveTyping } }
    @Published var guardFocus = Setting.guardFocus { didSet { Setting.guardFocus = guardFocus } }
    @Published var typingInterval = Double(Setting.typingInterval) { didSet { Setting.typingInterval = Int(typingInterval) } }
    @Published var language = Setting.language { didSet { Setting.language = language } }
    @Published var useBuiltinKeyterm = Setting.useBuiltinKeyterm { didSet { Setting.useBuiltinKeyterm = useBuiltinKeyterm } }
    @Published var showFloatingButton = Setting.showFloatingButton { didSet { Setting.showFloatingButton = showFloatingButton } }
    @Published var playSound = Setting.playSound { didSet { Setting.playSound = playSound } }
    @Published var launchAtLogin = Setting.launchAtLogin { didSet { Setting.launchAtLogin = launchAtLogin } }

    /// Edited as free text, one term per line — far less fiddly than a table.
    @Published var keytermText = Setting.customKeyterm.joined(separator: "\n")

    func commitKeyterm() {
        Setting.customKeyterm = keytermText
            .components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }

    var keytermBudget: Int {
        AnthropicSpeechBackend.packKeyterm(Setting.keyterm).count
    }
}

private struct SettingView: View {

    @StateObject private var model = SettingModel()
    @State private var hotkeyLabel = Hotkey.describe()
    @State private var isTrusted = TextInserter.isTrusted
    @State private var micStatus = AVCaptureDevice.authorizationStatus(for: .audio)
    @State private var credentialNote = ""

    var body: some View {
        TabView {
            dictation.tabItem { Label("Dictation", systemImage: "mic") }
            vocabulary.tabItem { Label("Vocabulary", systemImage: "text.book.closed") }
            general.tabItem { Label("General", systemImage: "gearshape") }
            status.tabItem { Label("Status", systemImage: "checkmark.seal") }
        }
        .padding(16)
        .frame(width: 460, height: 620)
    }

    // MARK: - Dictation

    private var dictation: some View {
        Form {
            Section {
                Toggle("Type as I speak", isOn: $model.liveTyping)
                Text("Words appear while you talk and correct themselves as the recogniser revises them. Off buffers the whole take and pastes it once at the end.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section {
                Toggle("Only type into text fields", isOn: $model.guardFocus)
                Text("Live typing emits backspaces. This refuses to send them into surfaces that are not text — file lists, tables, canvases.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Typing speed") {
                Slider(value: $model.typingInterval, in: 200...5000, step: 100) {
                    Text("Delay between keystrokes")
                } minimumValueLabel: {
                    Text("Fast").font(.caption)
                } maximumValueLabel: {
                    Text("Safe").font(.caption)
                }
                Text("\(Int(model.typingInterval)) µs. Lower is snappier; too low and some Electron apps drop characters.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Language") {
                Picker("Recognise", selection: $model.language) {
                    ForEach(Setting.availableLanguage, id: \.code) { entry in
                        Text(entry.name).tag(entry.code)
                    }
                }
                .labelsHidden()
            }

            Section("Shortcut") {
                HotkeyRecorder(label: $hotkeyLabel)
                Text("Hold to talk, or tap to latch recording on. Press Escape while recording to cancel.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    // MARK: - Vocabulary

    private var vocabulary: some View {
        Form {
            Section("Your terms") {
                Text("One per line. Names, jargon and project words the recogniser would otherwise mangle. This is the single biggest accuracy win available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                TextEditor(text: $model.keytermText)
                    .font(.system(.body, design: .monospaced))
                    .frame(minHeight: 220)
                    .onChange(of: model.keytermText) { _, _ in model.commitKeyterm() }
            }

            Section {
                Toggle("Include built-in developer terms", isOn: $model.useBuiltinKeyterm)
                Text(Setting.builtinKeyterm.joined(separator: ", "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section {
                let used = model.keytermBudget
                LabeledContent("Budget used") {
                    Text("\(used) / 1024 characters")
                        .foregroundStyle(used > 1000 ? .orange : .secondary)
                }
                Text("The server caps the term list at 1024 characters. Anything past that is dropped.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    // MARK: - General

    private var general: some View {
        Form {
            Section("Interface") {
                Toggle("Show floating mic button", isOn: $model.showFloatingButton)
                Text("A round mic button that floats above every app. Drag to reposition; it never takes keyboard focus.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Toggle("Play a sound when dictation starts and stops", isOn: $model.playSound)
            }

            Section("Startup") {
                Toggle("Launch splaude at login", isOn: $model.launchAtLogin)
            }

            Section("Diagnostics") {
                Button("Reveal Log") {
                    NSWorkspace.shared.activateFileViewerSelecting([Diagnostic.path])
                }
                Text(Diagnostic.path.path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
        .formStyle(.grouped)
    }

    // MARK: - Status

    private var status: some View {
        Form {
            Section("Permission") {
                permissionRow(
                    "Accessibility",
                    granted: isTrusted,
                    detail: "Required to type into other apps.",
                    fix: {
                        TextInserter.requestTrust()
                        let pane = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
                    }
                )

                permissionRow(
                    "Microphone",
                    granted: micStatus == .authorized,
                    detail: "Required to hear you.",
                    fix: {
                        let pane = "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
                        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
                    }
                )
            }

            Section("Credential") {
                Text(credentialNote.isEmpty ? "Checking…" : credentialNote)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                Button("Re-check") { credentialNote = TokenStore.describe() }
            }

            Section("Connection") {
                Text("""
                    Speech streams to Anthropic's internal WebSocket, which runs \
                    Deepgram Nova-3 rather than a Claude model — so it does not \
                    spend Claude tokens. It is undocumented and can change \
                    without notice.
                    """)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                LabeledContent("Provider") { Text("deepgram-nova3").font(.caption.monospaced()) }
                LabeledContent("Quota headers") {
                    Text(QuotaWatch.summary).font(.caption.monospaced())
                }
                Text("Headers observed on the connection handshake. Claude-metered endpoints return anthropic-ratelimit-* headers; their absence is evidence this one is not on that meter.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            credentialNote = TokenStore.describe()
            isTrusted = TextInserter.isTrusted
            micStatus = AVCaptureDevice.authorizationStatus(for: .audio)
        }
    }

    private func permissionRow(_ title: String,
                               granted: Bool,
                               detail: String,
                               fix: @escaping () -> Void) -> some View {
        LabeledContent(title) {
            HStack(spacing: 8) {
                Image(systemName: granted ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                    .foregroundStyle(granted ? .green : .orange)
                if granted {
                    Text("Granted").foregroundStyle(.secondary)
                } else {
                    Button("Fix", action: fix)
                }
            }
        }
        .help(detail)
    }
}
// MARK: - Hotkey recorder

/// Captures the next key combination pressed.
///
/// Events arrive through a local monitor rather than `keyDown` on a first
/// responder. An NSView hosted in a SwiftUI `Form` is given neither a reliable
/// clickable frame — its intrinsic width is `noIntrinsicMetric`, which can
/// resolve to zero — nor a dependable place in the responder chain, so the
/// AppKit version of this control could sit there accepting nothing at all. A
/// monitor sees events before dispatch and is independent of both.
private struct HotkeyRecorder: View {

    @Binding var label: String

    @State private var isListening = false
    @State private var monitor: Any?
    @State private var holding: UInt32 = 0

    var body: some View {
        Button(action: toggle) {
            Text(display)
                .font(.system(size: 13, weight: .medium))
                .frame(maxWidth: .infinity, minHeight: 22)
                .contentShape(Rectangle())
        }
        .buttonStyle(.bordered)
        .tint(isListening ? Color.accentColor : nil)
        .help("Click, then press a combination such as ⌥⇧D. Escape cancels.")
        // A monitor outliving this view would swallow every keystroke in the
        // app, so it has to come down whenever the view goes away.
        .onDisappear(perform: stop)
    }

    private var display: String {
        guard isListening else { return label }
        return holding == 0 ? "Press a shortcut…" : Hotkey.describeModifier(holding) + "…"
    }

    private func toggle() {
        isListening ? stop() : start()
    }

    private func start() {
        guard monitor == nil else { return }
        isListening = true
        holding = 0

        // Carbon consumes the bound combo before anything else sees it, so
        // without dropping the grab the recorder cannot observe the very
        // shortcut most people want to rebind — it would start a take instead.
        Hotkey.active?.suspend()

        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .flagsChanged]) { event in
            handle(event)
        }
    }

    private func stop() {
        detach()
        guard isListening else { return }
        isListening = false
        holding = 0
        Hotkey.active?.resume()
    }

    private func detach() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
    }

    /// Returns nil to swallow the event: while listening the keys belong to the
    /// recorder and must not also reach whatever sits behind it.
    private func handle(_ event: NSEvent) -> NSEvent? {
        guard isListening else { return event }

        let carbon = Hotkey.carbonModifier(from: event.modifierFlags)

        // Show the combination building up as it is held, so a modifier-only
        // press looks like progress rather than a dead control.
        guard event.type != .flagsChanged else {
            holding = carbon
            return nil
        }

        // Escape abandons the recording rather than binding Escape.
        guard event.keyCode != UInt16(kVK_Escape) else {
            stop()
            return nil
        }

        // A bare key would fire constantly while typing.
        guard carbon != 0 else {
            NSSound.beep()
            return nil
        }

        Setting.hotkeyCode = UInt32(event.keyCode)
        Setting.hotkeyModifier = carbon
        label = Hotkey.describe()

        // Writing those settings posts the change notification, which
        // re-registers the hotkey from scratch — so no resume() here.
        detach()
        isListening = false
        holding = 0
        return nil
    }
}
