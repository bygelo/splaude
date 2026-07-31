import AppKit

/// Drops text into whatever app owns the keyboard focus, by borrowing the
/// pasteboard and synthesising Cmd-V. Works in every app, unlike accessibility
/// value injection, which many editors ignore.
enum TextInserter {

    /// The pasteboard is restored after this delay; the frontmost app needs a
    /// moment to actually read it before we put the old contents back.
    private static let restoreDelay: TimeInterval = 0.35

    static var isTrusted: Bool {
        AXIsProcessTrusted()
    }

    /// Shows the system prompt if this app has not been granted Accessibility yet.
    @discardableResult
    static func requestTrust() -> Bool {
        let option = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
        return AXIsProcessTrustedWithOptions(option as CFDictionary)
    }

    static func insert(_ text: String) {
        guard !text.isEmpty else { return }

        let pasteboard = NSPasteboard.general
        let saved = snapshot(pasteboard)

        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)

        paste()

        DispatchQueue.main.asyncAfter(deadline: .now() + restoreDelay) {
            restore(saved, to: pasteboard)
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
