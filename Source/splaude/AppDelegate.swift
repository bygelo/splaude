import AppKit
import Carbon.HIToolbox

final class AppDelegate: NSObject, NSApplicationDelegate {

    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let capture = AudioCapture()
    private let hotkey = Hotkey()
    private var floating: FloatingMic?

    /// Where the current take's text belongs. Nil when anchoring is off.
    private var anchor: FocusAnchor?
    /// Watches for Return while a take runs. Nil when not recording.
    private var submitMonitor: Any?

    /// Last known credential state, polled so it appears in the menu before a
    /// take fails rather than at the moment the hotkey is pressed.
    private var credentialHealth: TokenStore.Health = .usable(until: nil)
    private var healthTimer: Timer?
    /// Drift is reported once per take; a status update per frame would thrash.
    private var hasNotedDrift = false
    /// Return sent the message: the field is empty now, so nothing more of this take may be typed.
    private var hasSubmitted = false
    /// The user is typing into the field mid-take: the typer's picture of the screen is wrong,
    /// so it pauses. Once they stop for `resumeDelay` it starts a fresh segment at the cursor
    /// and keeps typing the words not yet on screen — never backspacing into what came before.
    /// Until 2026-11-25 the rest of the take went to the clipboard instead, and had to be pasted.
    private var userTookOver = false
    /// Spoken words already handed to earlier segments; the current segment types the rest.
    private var segmentBase = 0
    private var resumeTimer: Timer?
    /// The latest committed + interim text, what a resumed segment catches up to.
    private var lastDisplay = ""
    private static let resumeDelay: TimeInterval = 0.9

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

    /// Set while a take is waiting on a credential read that may be showing a
    /// login-password dialog. No second take starts while it is up.
    private var isUnlocking = false
    /// Whether the hotkey came back up during that wait, which is the normal
    /// case — you cannot hold a key and type a password at once.
    private var releasedWhileUnlocking = false

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

        // Warm the project bias in the background now, so the first take carries
        // it instead of triggering the harvest on the hotkey path.
        if Setting.useProjectKeyterm {
            let catalog = Setting.catalogPath
            Project.warm(catalog: catalog.isEmpty ? nil : URL(fileURLWithPath: catalog))
        }

        // Same reasoning for the credential: read it now, so the Keychain
        // prompt lands at launch rather than mid-thought on the first take.
        refreshCredentialHealth()
        healthTimer = Timer.scheduledTimer(withTimeInterval: 300, repeats: true) { [weak self] _ in
            self?.refreshCredentialHealth()
        }
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
        if isUnlocking {
            // A second press while the password dialog is up is the only way
            // out of a take nobody wants to finish authorising.
            cancelUnlock()
        } else if isRecording {
            // A second tap while latched ends the take.
            stopRecording()
        } else {
            startRecording()
        }
    }

    private func release(wasHold: Bool) {
        if isUnlocking {
            // This release belongs to a take that does not exist yet: the key
            // went up while the credential was still being unlocked. Remember
            // it, but do not let it cancel the take it was meant to start —
            // that is the bug where the mic appeared for an instant after the
            // password went in and had to be opened all over again.
            releasedWhileUnlocking = true
            return
        }
        guard wasHold, isRecording else { return }
        stopRecording()
    }

    // MARK: - Recording

    private func startRecording() {
        guard !isRecording, !isUnlocking else { return }

        // The common case: a token is already in hand, no dialog is possible,
        // and the take starts on this turn of the run loop.
        if let credential = TokenStore.unpromptedCredential() {
            openMic(with: credential)
            return
        }

        // Otherwise the Keychain has to be asked, and reading another app's
        // credential item can put up a login-password dialog. Asking inline
        // blocked the main thread until that dialog was answered, and the
        // hotkey release queued up behind it — so the take arrived and was
        // cancelled in the same instant the password went in. Ask off the main
        // thread instead, and let the take latch when the token lands.
        isUnlocking = true
        releasedWhileUnlocking = false
        status = "Unlocking credential…"
        buildMenu()

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let outcome = Result { try TokenStore.load() }

            DispatchQueue.main.async {
                // Cancelled, or superseded — the dialog can sit there for as
                // long as it takes someone to find their password.
                guard let self, self.isUnlocking else { return }
                self.isUnlocking = false

                switch outcome {
                case .success(let credential) where !credential.isExpired:
                    if self.releasedWhileUnlocking {
                        Diagnostic.log("credential", "unlocked after the hotkey came up — latching the take instead of dropping it")
                    }
                    self.openMic(with: credential)
                case .success:
                    // Read fine, but already dead — Claude Code is the only
                    // thing that can renew it.
                    self.abort(TokenStore.TokenError.expired.localizedDescription)
                    self.refreshCredentialHealth()

                case .failure(let error):
                    self.abort(error.localizedDescription)
                    // The take just failed on the credential, so the menu should
                    // say so rather than waiting for the next poll.
                    self.refreshCredentialHealth()
                }
            }
        }
    }

    /// Pulls up the microphone permission and opens the take. Split out of
    /// `startRecording` because the credential can arrive on either of two
    /// paths — already cached, or after a Keychain prompt — and everything from
    /// here on is identical.
    private func openMic(with credential: TokenStore.Credential) {
        capture.requestPermission { [weak self] granted in
            guard let self else { return }
            guard granted else {
                self.abort("Microphone access denied — enable it in System Settings › Privacy & Security › Microphone.")
                return
            }
            self.openStream(with: credential)
        }
    }

    /// Abandons a pending unlock. The dialog it put up is securityd's, so it
    /// stays on screen — answering it just hands back a token nobody uses.
    private func cancelUnlock() {
        isUnlocking = false
        releasedWhileUnlocking = false
        status = "Idle"
        buildMenu()
        render(level: 0)
    }

    private func openStream(with credential: TokenStore.Credential) {
        transcript.reset()
        undelivered = ""
        typer.reset()

        // Decide once, up front, where this take is going.
        let focus = FocusProbe.current()
        let focusAllows = Setting.guardFocus ? focus.acceptsTyping : true

        // Remote-desktop and VM clients re-encode keystrokes by keycode, and
        // LiveTyper's characters all ride on keycode 0. Classified here with
        // everything else about the take, because the typer's model of the
        // screen only holds if the delivery route cannot change mid-utterance.
        let bundle = FocusProbe.frontmostBundleIdentifier
        let retypesByKeycode = Setting.isPasteOnly(bundle)

        isTypingLive = Setting.liveTyping && TextInserter.isTrusted && focusAllows && !retypesByKeycode

        // Remember the field as well as the decision, so the rest of the take
        // can be held back if the user wanders off mid-sentence.
        anchor = Setting.anchorInput ? FocusAnchor.capture() : nil
        hasNotedDrift = false
        hasSubmitted = false
        userTookOver = false
        segmentBase = 0
        lastDisplay = ""
        resumeTimer?.invalidate()
        resumeTimer = nil
        watchForSubmit()

        // The bundle identifier is in here for one reason: a remote client the
        // paste-only list has never heard of delivers the take as a run of `a`,
        // and the only cure is adding its identifier to Settings. Printing it
        // on every take is what turns that from a guess into a copy and paste.
        Diagnostic.session("record — \(FocusProbe.frontmostApp) (\(bundle ?? "no bundle id")) / \(focus.label) → \(isTypingLive ? "live typing" : "paste at end")\(anchor == nil ? "" : ", anchored")")

        // Live typing going quiet with no explanation is exactly the failure
        // mode this app logs against, so say which app turned it off and why.
        if Setting.liveTyping && retypesByKeycode {
            Diagnostic.log("type", "\(FocusProbe.frontmostApp) (\(bundle ?? "no bundle id")) re-encodes keystrokes by keycode — buffering this take and pasting at the end")
            status = "\(FocusProbe.frontmostApp) needs pasted text — will paste at the end"
        } else if Setting.liveTyping && !focusAllows {
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

        unwatchSubmit()
        capture.stop()
        status = "Finishing…"
        backend?.finish()

        if Setting.playSound { NSSound(named: "Pop")?.play() }

        buildMenu()
        render(level: 0)
    }

    private func abort(_ message: String) {
        isRecording = false
        unwatchSubmit()
        capture.stop()
        backend?.finish()
        backend = nil

        status = message
        buildMenu()
        render(level: 0)

        NSSound.beep()
    }

    // MARK: - Delivery

    /// Whether keystrokes posted right now would land where the take started.
    /// Always true when anchoring is off — that is the follow-focus behaviour.
    private var canTypeNow: Bool {
        guard let anchor else { return true }
        return anchor.holdsFocus
    }

    // MARK: - Submit to finish

    /// Watches for Return so that submitting ends the take.
    ///
    /// A *global* monitor deliberately: it observes without consuming, so the
    /// keystroke still reaches the app and sends the message. Swallowing it
    /// would mean pressing Return did nothing but stop dictating, which is the
    /// opposite of what someone hitting Return wants.
    ///
    /// The same monitor sees the user's own typing. Either one means the screen no
    /// longer holds what LiveTyper typed, and a typer that kept going would type the
    /// late final transcript into the emptied box (the last words, twice) or
    /// backspace over the user's characters. So the typer halts on the spot.
    private func watchForSubmit() {
        guard submitMonitor == nil else { return }

        submitMonitor = NSEvent.addGlobalMonitorForEvents(matching: .keyDown) { [weak self] event in
            // LiveTyper tags every key it posts; those are not the user's.
            if event.cgEvent?.getIntegerValueField(.eventSourceUserData) == LiveTyper.mark { return }
            let key = Int(event.keyCode)
            let isSubmit = key == kVK_Return || key == kVK_ANSI_KeypadEnter
            // A shortcut that does not edit text (⌘C, ⌘Tab) leaves the field as it was.
            if !isSubmit, event.modifierFlags.contains(.command), ![kVK_ANSI_V, kVK_ANSI_X, kVK_ANSI_Z].contains(key) { return }
            if !isSubmit, key == Int(Setting.hotkeyCode) { return }

            guard let self, self.isRecording, !self.hasSubmitted else { return }
            if isSubmit, Setting.stopOnReturn {
                if self.isTypingLive { self.typer.halt() }
                self.hasSubmitted = true
                Diagnostic.log("submit", "Return pressed — ending the take, typing stopped")
                self.stopRecording()
                return
            }
            // A global monitor sees keys typed in *any* app. Keys typed while the anchored
            // field is not focused (the take is already holding its text) never touched it:
            // three of the twelve takeovers logged on 2026-11-25 were exactly that.
            guard self.isTypingLive, self.canTypeNow else { return }
            self.typer.halt()
            if !self.userTookOver {
                self.userTookOver = true
                Diagnostic.log("type", "user typed mid-take (key \(key)) — paused until they stop")
            }
            self.scheduleResume()
        }
    }

    /// Restarted by every user key, so the typer waits for a real pause.
    private func scheduleResume() {
        resumeTimer?.invalidate()
        resumeTimer = Timer.scheduledTimer(withTimeInterval: Self.resumeDelay, repeats: false) { [weak self] _ in
            self?.resumeAfterUser()
        }
    }

    /// Starts a new segment at the cursor. What the halted typer put on screen is
    /// counted as handed over; the user may have edited it, so it is never touched again.
    private func resumeAfterUser() {
        resumeTimer = nil
        guard isRecording, isTypingLive, userTookOver, !hasSubmitted else { return }
        segmentBase += wordCount(typer.snapshot())
        typer.reset()
        userTookOver = false
        Diagnostic.log("type", "user paused — typing resumes after word \(segmentBase)")
        guard canTypeNow else { noteDrift(); return }
        typer.update(to: segmentTail(lastDisplay))
    }

    private func wordCount(_ text: String) -> Int {
        text.split(whereSeparator: \.isWhitespace).count
    }

    /// The part of the transcript the current segment owns: every word past `segmentBase`,
    /// led by a space so it does not fuse with the word before the cursor.
    ///
    /// Counted in words, not characters: a revision that respells a word already handed
    /// over ("want" → "wanted") leaves the word count where it was, so the boundary holds.
    private func segmentTail(_ text: String) -> String {
        guard segmentBase > 0 else { return text }
        let word = text.split(whereSeparator: \.isWhitespace)
        guard word.count > segmentBase else { return "" }
        return " " + word[segmentBase...].joined(separator: " ")
    }

    private func unwatchSubmit() {
        if let submitMonitor { NSEvent.removeMonitor(submitMonitor) }
        submitMonitor = nil
    }

    // MARK: - Credential health

    /// Reads the credential off the main thread. The Keychain prompt is modal
    /// and blocks whoever asks, so doing this inline would freeze the menu
    /// until the dialog is answered.
    private func refreshCredentialHealth() {
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let health = TokenStore.health()
            DispatchQueue.main.async {
                guard let self else { return }
                let changed = health.headline != self.credentialHealth.headline
                self.credentialHealth = health
                if changed {
                    if let line = health.headline { Diagnostic.log("credential", line) }
                    self.buildMenu()
                }
            }
        }
    }

    private func noteDrift() {
        guard !hasNotedDrift, let anchor else { return }
        hasNotedDrift = true
        Diagnostic.log("anchor", "focus left \(anchor.appName) — holding text until it returns")
        status = "Paused — waiting for \(anchor.appName)"
        buildMenu()
    }

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

        // A take that ended somewhere else still belongs to the field it began
        // in, so route it there rather than pasting into whatever is in front.
        if let anchor, !anchor.holdsFocus {
            if anchor.insertDirectly(text) {
                Diagnostic.log("insert", "wrote \(text.count) chars into \(anchor.appName) directly")
                status = "Inserted into \(anchor.appName)"
                buildMenu()
                return
            }

            // No accessibility write available — Electron, terminals and web
            // views mostly refuse it. Pasting is the only route left, and a
            // paste goes to the front, so focus has to be handed back first.
            Diagnostic.log("insert", "\(anchor.appName) refused a direct write — returning focus to paste")
            anchor.reactivate()
            let pending = text
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
                TextInserter.insert(pending)
            }
            return
        }

        Diagnostic.log("insert", "pasting \(text.count) chars")
        TextInserter.insert(text)
    }

    /// The fix is a terminal command, so put it on the clipboard rather than
    /// asking someone to retype it from a menu they just dismissed.
    @objc private func showCredentialHelp() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("claude", forType: .string)

        let alert = NSAlert()
        alert.messageText = "Claude Code credential needs refreshing"
        alert.informativeText = """
            splaude reads the OAuth token Claude Code stores, but it never \
            refreshes that token — Claude Code does, and only while it runs.

            Run `claude` in a terminal to renew it, then dictate again. The \
            command is on your clipboard.
            """
        alert.alertStyle = .warning
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Re-check Now")

        NSApp.activate(ignoringOtherApps: true)
        if alert.runModal() == .alertSecondButtonReturn {
            TokenStore.invalidate()
            refreshCredentialHealth()
        }
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

        // Above the separator, so a dead credential is the first thing read
        // rather than something discovered when a take fails.
        if let warning = credentialHealth.headline {
            let item = NSMenuItem(title: warning, action: #selector(showCredentialHelp), keyEquivalent: "")
            item.target = self
            item.image = NSImage(systemSymbolName: "exclamationmark.triangle.fill",
                                 accessibilityDescription: nil)
            item.toolTip = "splaude reads the Claude Code credential but does not refresh it. Running `claude` renews it."
            menu.addItem(item)
        }

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

    /// Whether a callback belongs to the take now running. A finished take's close arrives
    /// up to `closeGrace` after its stop, and a hotkey press in that gap has already started
    /// the next take: on 2026-11-25 15:24:20 the old take's close ran 0.23 s into the new one,
    /// set `backend` to nil, stopped its mic and left its socket open for 12 minutes. A stale
    /// sender's state (transcript, typer, anchor) was already reset by the new take, so there
    /// is nothing of it left to deliver: drop the callback.
    private func isCurrent(_ sender: SpeechBackend, _ what: String) -> Bool {
        if sender === backend { return true }
        Diagnostic.log("take", "ignored \(what) from a finished take")
        return false
    }

    func speechDidOpen(_ sender: SpeechBackend) {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isCurrent(sender, "open"), self.isRecording else { return }
            self.status = "Listening"
            self.buildMenu()
        }
    }

    func speechDidTranscribe(_ sender: SpeechBackend, _ text: String, isFinal: Bool) {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isCurrent(sender, "transcript") else { return }

            let display = self.transcript.apply(text, isFinal: isFinal)
            self.lastDisplay = display
            self.statusItem.button?.toolTip = display

            // Live mode reconciles on every frame, interim included — that is
            // what makes words appear as you speak and correct themselves.
            if self.isTypingLive {
                // Return already sent what was on screen, or the user is typing there.
                guard !self.hasSubmitted, !self.userTookOver else { return }
                // Holding back is the whole point of the anchor: keystrokes go
                // wherever focus is when posted, so typing on regardless would
                // scatter the sentence — and its backspaces — into whatever the
                // user switched to. Nothing is lost by waiting. The typer keeps
                // its own record of what it emitted, so when focus comes back
                // the next frame diffs against that and types the whole gap.
                guard self.canTypeNow else {
                    self.noteDrift()
                    return
                }

                self.typer.update(to: self.segmentTail(display))
                if isFinal { self.typer.lock() }
                return
            }

            guard isFinal else { return }

            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { return }
            self.undelivered = self.undelivered.isEmpty ? trimmed : "\(self.undelivered) \(trimmed)"
        }
    }

    func speechDidFail(_ sender: SpeechBackend, _ message: String, fatal: Bool) {
        Diagnostic.log("stt", "fail (fatal: \(fatal)): \(message)")
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isCurrent(sender, "failure") else { return }
            if fatal || self.isRecording {
                self.abort(message)
            } else {
                self.status = message
                self.buildMenu()
            }
        }
    }

    func speechDidClose(_ sender: SpeechBackend) {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isCurrent(sender, "close") else { return }

            self.backend = nil
            self.isRecording = false
            self.capture.stop()

            let wordCount = self.transcript.committed.split(separator: " ").count

            if self.isTypingLive {
                // Usually already on screen, typed as it was spoken — but
                // anything held back while focus was away never got there, and
                // ending the take is the last chance to place it.
                self.resumeTimer?.invalidate()
                self.resumeTimer = nil
                if self.userTookOver {
                    // Still paused when the take ended: hand over what the halted typer
                    // reached, exactly as a resume would, and paste the rest below.
                    self.segmentBase += self.wordCount(self.typer.snapshot())
                    self.typer.reset()
                }
                let onScreen = self.userTookOver ? "" : self.typer.snapshot()
                let spoken = self.segmentTail(self.transcript.committed)

                let held: String
                if self.hasSubmitted {
                    // Return sent the message and emptied the field; whatever the
                    // recogniser revised after that is not typed into the empty box.
                    held = ""
                    if spoken != onScreen { Diagnostic.log("submit", "\(spoken.count - onScreen.count) chars of late transcript not typed after Return") }
                } else if self.userTookOver {
                    held = spoken
                    if !held.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                        Diagnostic.log("type", "take ended while the user was typing — pasting the last \(held.count) chars at the cursor")
                    }
                } else if spoken.hasPrefix(onScreen) {
                    held = String(spoken.dropFirst(onScreen.count))
                } else {
                    // A revision rewrote text the typer had already emitted, so
                    // the two no longer line up and any remainder computed from
                    // them would duplicate words. Leave it alone.
                    held = ""
                    if onScreen != spoken {
                        Diagnostic.log("anchor", "typed text diverged from transcript — nothing recovered")
                    }
                }

                self.isTypingLive = false
                self.typer.reset()

                if !held.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    Diagnostic.log("anchor", "delivering \(held.count) chars held while focus was away")
                    self.deliver(held)
                }
            } else {
                let pending = self.undelivered
                self.undelivered = ""
                if !pending.isEmpty { self.deliver(pending) }
            }

            self.anchor = nil
            self.hasNotedDrift = false
            self.hasSubmitted = false
            self.userTookOver = false
            self.unwatchSubmit()

            if ["Finishing…", "Listening", "Connecting…"].contains(self.status) {
                self.status = wordCount == 0 ? "Idle" : "\(wordCount) words"
            }
            // A take that heard nothing at all is a muted input, not a quiet speaker:
            // on 2026-11-24 the Mac's input volume sat at 0 and every take came back blank.
            if wordCount == 0, self.capture.lastPeak < 0.01 {
                Diagnostic.log("audio", "silent take — input volume 0 or the wrong input device")
                self.status = "No sound from the mic — check Sound › Input"
            }

            self.buildMenu()
            self.render(level: 0)
        }
    }
}
