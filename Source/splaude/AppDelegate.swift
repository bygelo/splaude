import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {

    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let capture = AudioCapture()
    private let hotkey = Hotkey()
    private var floating: FloatingMic?

    private var backend: (any SpeechBackend)?
    private var transcript = TranscriptBuffer()
    private let typer = LiveTyper()

    /// Whether this take is typing live, decided once at the start from what
    /// held focus — it must not flip mid-utterance or the typer's model of the
    /// screen stops matching the screen.
    private var isTypingLive = false
    /// Text committed but not yet pasted, used only when not typing live.
    private var undelivered = ""
    private var isRecording = false
    private var status = "Idle"

    // MARK: - Lifecycle

    func applicationDidFinishLaunching(_ notification: Notification) {
        Diagnostic.session("launch — accessibility \(TextInserter.isTrusted ? "granted" : "NOT granted")")
        buildMenu()
        render(level: 0)

        applyHotkey()

        hotkey.onEngage = { [weak self] in self?.engage() }
        hotkey.onRelease = { [weak self] wasHold in self?.release(wasHold: wasHold) }

        capture.onAudio = { [weak self] data in self?.backend?.send(audio: data) }
        capture.onLevel = { [weak self] level in self?.render(level: level) }
        capture.onFailure = { [weak self] message in self?.abort(message) }

        applyFloatingButton()

        NotificationCenter.default.addObserver(
            self, selector: #selector(settingDidChange),
            name: Setting.didChange, object: nil
        )

        // Ask for both permissions up front so the first dictation is not
        // interrupted by two system dialogs.
        capture.requestPermission { _ in }
        if !TextInserter.isTrusted { TextInserter.requestTrust() }
    }

    func applicationWillTerminate(_ notification: Notification) {
        hotkey.unregister()
        capture.stop()
        backend?.finish()
    }

    // MARK: - Configuration

    @objc private func settingDidChange() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.applyHotkey()
            self.applyFloatingButton()
            self.buildMenu()
        }
    }

    private func applyHotkey() {
        if hotkey.register() {
            if status.hasPrefix("Hotkey") { status = "Idle" }
        } else {
            status = "Hotkey \(Hotkey.describe()) is already taken"
        }
    }

    private func applyFloatingButton() {
        guard Setting.showFloatingButton else {
            floating?.orderOut(nil)
            floating = nil
            return
        }

        guard floating == nil else { return }

        let panel = FloatingMic()
        panel.onToggle = { [weak self] in self?.toggle() }
        panel.set(recording: isRecording)
        panel.orderFrontRegardless()
        floating = panel
    }

    // MARK: - Hotkey handling

    /// Tap latches; hold dictates for as long as the key is down.
    private func engage() {
        if isRecording {
            // A second tap while latched ends the take.
            stopRecording()
        } else {
            startRecording()
        }
    }

    private func release(wasHold: Bool) {
        guard wasHold, isRecording else { return }
        stopRecording()
    }

    // MARK: - Recording

    private func startRecording() {
        guard !isRecording else { return }

        let credential: TokenStore.Credential
        do {
            credential = try TokenStore.load()
            guard !credential.isExpired else { throw TokenStore.TokenError.expired }
        } catch {
            abort(error.localizedDescription)
            return
        }

        capture.requestPermission { [weak self] granted in
            guard let self else { return }
            guard granted else {
                self.abort("Microphone access denied — enable it in System Settings › Privacy & Security › Microphone.")
                return
            }
            self.openStream(with: credential)
        }
    }

    private func openStream(with credential: TokenStore.Credential) {
        transcript.reset()
        undelivered = ""
        typer.reset()

        // Decide once, up front, where this take is going.
        let focus = FocusProbe.current()
        let focusAllows = Setting.guardFocus ? focus.acceptsTyping : true
        isTypingLive = Setting.liveTyping && TextInserter.isTrusted && focusAllows
        Diagnostic.session("record — \(FocusProbe.frontmostApp) / \(focus.label) → \(isTypingLive ? "live typing" : "paste at end")")

        if Setting.liveTyping && !focusAllows {
            status = "\(FocusProbe.frontmostApp) has no text field — will paste at the end"
        }

        if Setting.playSound { NSSound(named: "Tink")?.play() }

        let backend = AnthropicSpeechBackend(credential: credential)
        backend.delegate = self
        self.backend = backend

        isRecording = true
        status = "Connecting…"
        backend.start()

        capture.start(format: AnthropicSpeechBackend.audioFormat)
        buildMenu()
        render(level: 0)
    }

    private func stopRecording() {
        guard isRecording else { return }
        isRecording = false

        capture.stop()
        status = "Finishing…"
        backend?.finish()

        if Setting.playSound { NSSound(named: "Pop")?.play() }

        buildMenu()
        render(level: 0)
    }

    private func abort(_ message: String) {
        isRecording = false
        capture.stop()
        backend?.finish()
        backend = nil

        status = message
        buildMenu()
        render(level: 0)

        NSSound.beep()
    }

    // MARK: - Delivery

    private func deliver(_ text: String) {
        guard !text.isEmpty else { return }

        guard TextInserter.isTrusted else {
            // The words are already earned — park them on the clipboard so a
            // missing permission costs a Cmd-V, not the whole take.
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)

            Diagnostic.log("insert", "blocked — no Accessibility grant; \(text.count) chars left on clipboard")
            status = "Not allowed to paste — text is on your clipboard"
            buildMenu()
            promptForAccessibility()
            return
        }

        Diagnostic.log("insert", "pasting \(text.count) chars")
        TextInserter.insert(text)
    }

    /// Shown at most once per launch — a modal every utterance would be worse
    /// than the bug.
    private var hasPromptedForAccessibility = false

    private func promptForAccessibility() {
        guard !hasPromptedForAccessibility else { return }
        hasPromptedForAccessibility = true

        TextInserter.requestTrust()

        let alert = NSAlert()
        alert.messageText = "splaude needs Accessibility access"
        alert.informativeText = """
            Transcription is working — you can see it in the menu bar — but macOS \
            blocks splaude from pasting into other apps until you allow it under \
            Privacy & Security › Accessibility.

            Turn splaude on in that list, then dictate again. This take is already \
            on your clipboard.
            """
        alert.addButton(withTitle: "Open System Settings")
        alert.addButton(withTitle: "Later")
        alert.alertStyle = .warning

        NSApp.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        let pane = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
    }

    // MARK: - Menu bar

    private func render(level: Float) {
        floating?.set(recording: isRecording)
        floating?.set(level: level)

        guard let button = statusItem.button else { return }

        // While recording the icon doubles as a level meter, so a dead mic is
        // visible at a glance rather than after a silent failed take.
        let name = isRecording
            ? (level < 0.15 ? "mic.fill" : "mic.and.signal.meter.fill")
            : "mic"

        button.image = NSImage(systemSymbolName: name, accessibilityDescription: "splaude")
        button.image?.isTemplate = !isRecording
        button.contentTintColor = isRecording ? .systemRed : nil
        button.toolTip = isRecording ? "Recording — \(Hotkey.describe()) to stop" : status
    }

    private func buildMenu() {
        let menu = NSMenu()

        let heading = NSMenuItem(title: isRecording ? "Recording…" : Self.clip(status),
                                 action: nil, keyEquivalent: "")
        heading.isEnabled = false
        heading.toolTip = status
        menu.addItem(heading)

        menu.addItem(.separator())

        if !transcript.committed.isEmpty {
            // A fixed title rather than the transcript itself — menu width grows
            // to fit its longest item, and a sentence of dictation stretches the
            // whole menu across the screen.
            let copy = NSMenuItem(title: "Copy Last Transcript", action: #selector(copyTranscript), keyEquivalent: "")
            copy.target = self
            copy.toolTip = transcript.committed
            menu.addItem(copy)
        }

        let dictate = NSMenuItem(title: isRecording ? "Stop Dictation" : "Start Dictation",
                                 action: #selector(toggle), keyEquivalent: "")
        dictate.target = self
        menu.addItem(dictate)

        let shortcut = NSMenuItem(title: "\(Hotkey.describe()) — hold or tap", action: nil, keyEquivalent: "")
        shortcut.isEnabled = false
        shortcut.toolTip = "Hold to talk, or tap to latch recording on"
        menu.addItem(shortcut)

        menu.addItem(.separator())

        // Exercises the paste path alone, with no microphone or network in the
        // way — the fastest way to tell which half is broken.
        let test = NSMenuItem(title: "Test Paste", action: #selector(testPaste), keyEquivalent: "")
        test.target = self
        test.toolTip = "Types a test phrase into the app you were last using"
        menu.addItem(test)

        let trusted = TextInserter.isTrusted
        let permission = NSMenuItem(
            title: trusted ? "Accessibility: granted" : "Accessibility: NOT granted — click to fix",
            action: trusted ? nil : #selector(fixAccessibility),
            keyEquivalent: ""
        )
        permission.target = self
        permission.isEnabled = !trusted
        menu.addItem(permission)

        let log = NSMenuItem(title: "Reveal Log", action: #selector(revealLog), keyEquivalent: "")
        log.target = self
        menu.addItem(log)

        menu.addItem(.separator())

        let setting = NSMenuItem(title: "Settings…", action: #selector(openSetting), keyEquivalent: ",")
        setting.target = self
        menu.addItem(setting)
        let quit = NSMenuItem(title: "Quit splaude", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        menu.addItem(quit)

        statusItem.menu = menu
    }

    /// A menu is as wide as its widest item, so nothing variable-length goes in
    /// one untruncated. The full text lives in the tooltip.
    private static func clip(_ text: String, to limit: Int = 38) -> String {
        text.count <= limit ? text : String(text.prefix(limit - 1)) + "…"
    }

    @objc private func toggle() {
        isRecording ? stopRecording() : startRecording()
    }

    @objc private func openSetting() {
        SettingWindow.shared.show()
    }

    /// Fires after a short delay so the menu can close and focus can return to
    /// the app you were actually typing in.
    @objc private func testPaste() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            self?.deliver("splaude test paste")
        }
    }

    @objc private func fixAccessibility() {
        hasPromptedForAccessibility = false
        promptForAccessibility()
    }

    @objc private func revealLog() {
        NSWorkspace.shared.activateFileViewerSelecting([Diagnostic.path])
    }

    @objc private func copyTranscript() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(transcript.committed, forType: .string)
    }
}

// MARK: - SpeechBackendDelegate

extension AppDelegate: SpeechBackendDelegate {

    func speechDidOpen() {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isRecording else { return }
            self.status = "Listening"
            self.buildMenu()
        }
    }

    func speechDidTranscribe(_ text: String, isFinal: Bool) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }

            let display = self.transcript.apply(text, isFinal: isFinal)
            self.statusItem.button?.toolTip = display

            // Live mode reconciles on every frame, interim included — that is
            // what makes words appear as you speak and correct themselves.
            if self.isTypingLive {
                self.typer.update(to: display)
                if isFinal { self.typer.lock() }
                return
            }

            guard isFinal else { return }

            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { return }
            self.undelivered = self.undelivered.isEmpty ? trimmed : "\(self.undelivered) \(trimmed)"
        }
    }

    func speechDidFail(_ message: String, fatal: Bool) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            if fatal || self.isRecording {
                self.abort(message)
            } else {
                self.status = message
                self.buildMenu()
            }
        }
    }

    func speechDidClose() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }

            self.backend = nil
            self.isRecording = false
            self.capture.stop()

            let wordCount = self.transcript.committed.split(separator: " ").count

            if self.isTypingLive {
                // Already on screen, typed as it was spoken.
                self.isTypingLive = false
                self.typer.reset()
            } else {
                let pending = self.undelivered
                self.undelivered = ""
                if !pending.isEmpty { self.deliver(pending) }
            }

            if ["Finishing…", "Listening", "Connecting…"].contains(self.status) {
                self.status = wordCount == 0 ? "Idle" : "\(wordCount) words"
            }

            self.buildMenu()
            self.render(level: 0)
        }
    }
}
