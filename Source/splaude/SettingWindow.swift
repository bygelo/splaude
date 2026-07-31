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

/// Captures the next key combination pressed. AppKit rather than SwiftUI because
/// it needs raw keyDown with modifier flags, before any text interpretation.
private struct HotkeyRecorder: NSViewRepresentable {

    @Binding var label: String

    func makeNSView(context: Context) -> RecorderView {
        let view = RecorderView()
        view.onRecord = { code, modifier in
            Setting.hotkeyCode = code
            Setting.hotkeyModifier = modifier
            label = Hotkey.describe()
        }
        return view
    }

    func updateNSView(_ view: RecorderView, context: Context) {
        view.label = label
        view.needsDisplay = true
    }

    final class RecorderView: NSView {

        var onRecord: ((UInt32, UInt32) -> Void)?
        var label = Hotkey.describe()
        private var isListening = false

        override var acceptsFirstResponder: Bool { true }
        override var intrinsicContentSize: NSSize { NSSize(width: NSView.noIntrinsicMetric, height: 28) }

        override func mouseDown(with event: NSEvent) {
            isListening = true
            // Carbon eats the bound combo before the responder chain sees it,
            // so without this the recorder cannot observe the very shortcut a
            // user is most likely to rebind — it would start a take instead.
            Hotkey.active?.suspend()
            window?.makeFirstResponder(self)
            needsDisplay = true
        }

        /// Every path out of listening has to land here, or the suspend above
        /// leaves the app with no working hotkey at all.
        private func stopListening() {
            guard isListening else { return }
            isListening = false
            Hotkey.active?.resume()
            needsDisplay = true
        }

        override func resignFirstResponder() -> Bool {
            stopListening()
            return super.resignFirstResponder()
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window == nil { stopListening() }
        }

        override func keyDown(with event: NSEvent) {
            guard isListening else { return super.keyDown(with: event) }

            // Escape abandons the recording rather than binding Escape.
            guard event.keyCode != UInt16(kVK_Escape) else {
                stopListening()
                return
            }

            var carbon: UInt32 = 0
            if event.modifierFlags.contains(.command) { carbon |= UInt32(cmdKey) }
            if event.modifierFlags.contains(.option) { carbon |= UInt32(optionKey) }
            if event.modifierFlags.contains(.control) { carbon |= UInt32(controlKey) }
            if event.modifierFlags.contains(.shift) { carbon |= UInt32(shiftKey) }

            // A bare key would fire constantly while typing.
            guard carbon != 0 else { NSSound.beep(); return }

            isListening = false
            // Writing the setting posts the change notification, which
            // re-registers the hotkey from scratch — so no resume() here.
            onRecord?(UInt32(event.keyCode), carbon)
            needsDisplay = true
        }

        override func draw(_ dirtyRect: NSRect) {
            let rounded = NSBezierPath(roundedRect: bounds.insetBy(dx: 1, dy: 1), xRadius: 6, yRadius: 6)

            (isListening ? NSColor.controlAccentColor.withAlphaComponent(0.15) : NSColor.controlColor).setFill()
            rounded.fill()

            (isListening ? NSColor.controlAccentColor : NSColor.separatorColor).setStroke()
            rounded.lineWidth = 1
            rounded.stroke()

            let text = isListening ? "Press a shortcut…" : label
            let style = NSMutableParagraphStyle()
            style.alignment = .center

            let attribute: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: 13, weight: .medium),
                .foregroundColor: isListening ? NSColor.controlAccentColor : NSColor.labelColor,
                .paragraphStyle: style,
            ]

            let size = (text as NSString).size(withAttributes: attribute)
            let rect = NSRect(x: 0, y: bounds.midY - size.height / 2, width: bounds.width, height: size.height)
            (text as NSString).draw(in: rect, withAttributes: attribute)
        }
    }
}
