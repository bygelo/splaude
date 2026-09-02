import AppKit

/// Drops text into whatever app owns the keyboard focus, by borrowing the
/// pasteboard and synthesising Cmd-V. Works in every app, unlike accessibility
/// value injection, which many editors ignore.
///
/// There are two chords, not one, and the difference is the whole
/// remote-desktop story. A local app is handed our `CGEvent` and reads what is
/// on it, so a V carrying `.maskCommand` is a complete ⌘V. A remote-desktop,
/// VM or screen-sharing client is not a reader but a *translator*: it watches
/// the keyboard the way hardware would — modifier state from the ⌘ key's own
/// events, the letter from the keycode — re-encodes that into a scancode, and
/// ships it down the wire. Everything the local chord relies on is invisible to
/// it, so a take pasted into a remote window arrives as a lone `v`. `paste()`
/// is the local chord and has not changed; `pasteHoldingCommand()` is the one
/// shaped like hardware, and `Setting.pasteOnlyApp` decides which is used.
enum TextInserter {

    /// The pasteboard is restored after this delay; the frontmost app needs a
    /// moment to actually read it before we put the old contents back.
    private static let restoreDelay: TimeInterval = 0.35

    /// The same wait, for a remote-desktop or screen-sharing client.
    ///
    /// A local app reads the pasteboard synchronously inside its own paste, so
    /// a third of a second is generous. A redirected clipboard is three hops
    /// instead of one: the client has to *notice* the pasteboard changed — it
    /// polls `changeCount` — then announce the new format list to the far end,
    /// and only then does the remote application ask for the bytes, after the
    /// ⌘V has already been delivered. Restore on the local schedule and the
    /// answer to that request is the user's *old* clipboard. That is the
    /// failure that looks like nothing happened at all: the paste fires, and
    /// either nothing appears or whatever was copied before the take does.
    private static let remoteRestoreDelay: TimeInterval = 2.0

    /// How long the clipboard is left alone *before* the chord goes out, for a
    /// remote client. The same reason from the other end: the far side cannot
    /// paste a payload the client has not announced yet, so the paste has to be
    /// later than the copy by more than a frame.
    private static let remoteClipboardLead: TimeInterval = 0.25

    /// Microseconds between the synthetic events of the remote paste chord.
    ///
    /// Zero is right locally — the events are queued in order and the window
    /// server hands them to the app as posted. A remote client is sampling
    /// instead: it reads keyboard state, encodes a scancode and ships it, so
    /// four events posted inside the same microsecond can be coalesced or
    /// re-encoded out of order, and a ⌘ that arrives after its V is not a paste.
    private static let remoteKeyGap: useconds_t = 12_000

    /// The remote chord blocks on `usleep`, and every caller is the main thread.
    private static let remoteQueue = DispatchQueue(label: "com.bygelo.splaude.paste")

    static var isTrusted: Bool {
        AXIsProcessTrusted()
    }

    /// Shows the system prompt if this app has not been granted Accessibility yet.
    @discardableResult
    static func requestTrust() -> Bool {
        let option = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
        return AXIsProcessTrustedWithOptions(option as CFDictionary)
    }

    /// Asks the frontmost app which delivery it needs.
    ///
    /// Read here rather than inherited from the decision the take made when it
    /// started, because a paste lands wherever focus is *now* — after an anchor
    /// hands focus back that is not always where the take began — and the
    /// question this answers is about the destination, not the origin.
    static func insert(_ text: String) {
        insert(text, isRemoteClient: Setting.isPasteOnly(FocusProbe.frontmostBundleIdentifier))
    }

    /// `isRemoteClient` selects the slower, hardware-shaped chord below. A
    /// parameter rather than a lookup so the two deliveries can be driven from
    /// a test or a diagnostic without a remote desktop in front of them.
    static func insert(_ text: String, isRemoteClient: Bool) {
        guard !text.isEmpty else { return }

        let pasteboard = NSPasteboard.general
        let saved = snapshot(pasteboard)

        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)

        guard isRemoteClient else {
            paste()

            DispatchQueue.main.asyncAfter(deadline: .now() + restoreDelay) {
                restore(saved, to: pasteboard)
            }
            return
        }

        Diagnostic.log(
            "insert",
            "remote client — holding ⌘ for the paste, after \(Int(remoteClipboardLead * 1000)) ms of clipboard lead"
        )

        remoteQueue.asyncAfter(deadline: .now() + remoteClipboardLead) {
            pasteHoldingCommand()

            DispatchQueue.main.asyncAfter(deadline: .now() + remoteRestoreDelay) {
                restore(saved, to: pasteboard)
            }
        }
    }

    // MARK: - Keystroke

    private static func paste() {
        guard let source = CGEventSource(stateID: .combinedSessionState) else { return }

        // Suppress this app's own hotkey handling while the synthetic keys fly.
        source.setLocalEventsFilterDuringSuppressionState(
            [.permitLocalMouseEvents, .permitSystemDefinedEvents],
            state: .eventSuppressionStateSuppressionInterval
        )

        let v: CGKeyCode = 9  // kVK_ANSI_V

        guard let down = CGEvent(keyboardEventSource: source, virtualKey: v, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: v, keyDown: false) else { return }

        down.flags = .maskCommand
        up.flags = .maskCommand

        down.post(tap: .cgAnnotatedSessionEventTap)
        up.post(tap: .cgAnnotatedSessionEventTap)
    }

    /// The same paste, with ⌘ posted as an event of its own rather than only as
    /// a flag on the V.
    ///
    /// This is the remote-desktop bug, and it is the mirror image of the one
    /// `Setting.pasteOnlyApp` already documents. A native app reads
    /// `CGEvent.flags` off the keystroke, so `.maskCommand` on the V is a
    /// complete ⌘V and no modifier event is needed — which is why the local
    /// path above has shipped without one. A remote client does not read our
    /// event at all: it tracks modifier state from the ⌘ key going down and up,
    /// translates the keycode, and ships a scancode. It never saw a ⌘ go down,
    /// so what it ships is a bare `v` — the take is replaced in the remote
    /// window by a single letter. A real modifier event is the only thing that
    /// survives that translation, exactly as a real keycode is the only thing
    /// that survives it for the key itself.
    ///
    /// `.flagsChanged` rather than a key-down, because that is the event type a
    /// physical ⌘ generates and the type a client watching for one watches for.
    ///
    /// Posted to `.cghidEventTap` rather than the annotated session tap because
    /// a screen-sharing client generally reads the keyboard through an event
    /// tap of its own, and the HID tap is the point every tap on the machine is
    /// downstream of. The session tap reaches the focused application but
    /// bypasses the taps installed ahead of it, so the client would be handed
    /// nothing to re-encode.
    private static func pasteHoldingCommand() {
        guard let source = CGEventSource(stateID: .combinedSessionState) else { return }

        source.setLocalEventsFilterDuringSuppressionState(
            [.permitLocalMouseEvents, .permitSystemDefinedEvents],
            state: .eventSuppressionStateSuppressionInterval
        )

        let command: CGKeyCode = 55  // kVK_Command
        let v: CGKeyCode = 9  // kVK_ANSI_V

        guard let commandDown = CGEvent(keyboardEventSource: source, virtualKey: command, keyDown: true),
              let down = CGEvent(keyboardEventSource: source, virtualKey: v, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: v, keyDown: false),
              let commandUp = CGEvent(keyboardEventSource: source, virtualKey: command, keyDown: false)
        else { return }

        commandDown.type = .flagsChanged
        commandUp.type = .flagsChanged

        commandDown.flags = .maskCommand
        down.flags = .maskCommand
        up.flags = .maskCommand
        // Cleared, not left as ⌘: this event is what tells the far end the
        // modifier is up, and a ⌘ stuck down on the remote machine turns the
        // user's next keypress there into a shortcut.
        commandUp.flags = []

        let tap = CGEventTapLocation.cghidEventTap

        commandDown.post(tap: tap)
        usleep(remoteKeyGap)
        down.post(tap: tap)
        usleep(remoteKeyGap)
        up.post(tap: tap)
        usleep(remoteKeyGap)
        commandUp.post(tap: tap)
    }

    // MARK: - Pasteboard preservation

    private struct Snapshot {
        let item: [[NSPasteboard.PasteboardType: Data]]
    }

    private static func snapshot(_ pasteboard: NSPasteboard) -> Snapshot {
        let item = (pasteboard.pasteboardItems ?? []).map { source in
            var payload: [NSPasteboard.PasteboardType: Data] = [:]
            for type in source.types {
                if let data = source.data(forType: type) { payload[type] = data }
            }
            return payload
        }
        return Snapshot(item: item)
    }

    private static func restore(_ snapshot: Snapshot, to pasteboard: NSPasteboard) {
        pasteboard.clearContents()
        guard !snapshot.item.isEmpty else { return }

        let restored = snapshot.item.map { payload -> NSPasteboardItem in
            let item = NSPasteboardItem()
            for (type, data) in payload {
                item.setData(data, forType: type)
            }
            return item
        }

        pasteboard.writeObjects(restored)
    }
}
