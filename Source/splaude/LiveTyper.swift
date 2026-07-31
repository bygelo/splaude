import AppKit

/// Types transcription into the focused app as it arrives, and rewrites what it
/// already typed when the recogniser revises its guess.
///
/// The recogniser streams provisional text that mutates until an utterance ends
/// ("low testing" → "one, two, three"), so this keeps a copy of exactly what it
/// emitted, diffs each new target against it, backspaces only the characters
/// that actually changed, and types the rest.
///
/// The safety property is `locked`: text belonging to a *finished* utterance is
/// never backspaced over, so a revision can never chew backwards into words the
/// user typed themselves.
final class LiveTyper {

    /// What this object believes is on screen, past the insertion point.
    private var typed = ""
    /// Prefix length of `typed` that is committed and must never be rewritten.
    private var locked = 0

    private let queue = DispatchQueue(label: "com.bygelo.splaude.type")

    /// Longest run of backspaces to accept before giving up and leaving the
    /// text alone — a runaway diff must never machine-gun the delete key.
    private static let maxRewrite = 240
    /// UTF-16 units per synthetic event.
    private static let chunkSize = 16

    var isEmpty: Bool { typed.isEmpty }
    var text: String { typed }

    func reset() {
        queue.async { [weak self] in
            self?.typed = ""
            self?.locked = 0
        }
    }

    /// Marks everything typed so far as final.
    func lock() {
        queue.async { [weak self] in
            guard let self else { return }
            self.locked = self.typed.count
        }
    }

    /// Reconciles the screen with `target`, typing and backspacing the minimum.
    func update(to target: String) {
        queue.async { [weak self] in
            guard let self else { return }

            let current = Array(self.typed)
            let desired = Array(target)

            var shared = 0
            while shared < current.count, shared < desired.count, current[shared] == desired[shared] {
                shared += 1
            }

            // Never rewrite committed text, even if the diff wants to.
            let floor = min(self.locked, min(current.count, desired.count))
            let boundary = max(shared, floor)

            let removeCount = current.count - boundary
            let addition = String(desired[min(boundary, desired.count)...])

            guard removeCount > 0 || !addition.isEmpty else { return }

            guard removeCount <= Self.maxRewrite else {
                Diagnostic.log("type", "refusing \(removeCount)-character rewrite — resyncing instead")
                self.typed = target
                return
            }

            guard let source = CGEventSource(stateID: .combinedSessionState) else { return }

            if removeCount > 0 { self.backspace(removeCount, source: source) }
            if !addition.isEmpty { self.type(addition, source: source) }

            self.typed = String(desired[0..<min(boundary, desired.count)]) + addition
        }
    }

    // MARK: - Synthetic keys

    private func backspace(_ count: Int, source: CGEventSource) {
        let delete: CGKeyCode = 51  // kVK_Delete

        for _ in 0..<count {
            guard let down = CGEvent(keyboardEventSource: source, virtualKey: delete, keyDown: true),
                  let up = CGEvent(keyboardEventSource: source, virtualKey: delete, keyDown: false) else { return }

            // Push-to-talk means Option is very likely physically held right
            // now, and Option-Delete deletes a whole word. Clear the hardware
            // modifier state off every synthetic event.
            down.flags = []
            up.flags = []

            down.post(tap: .cgAnnotatedSessionEventTap)
            up.post(tap: .cgAnnotatedSessionEventTap)
            usleep(useconds_t(Setting.typingInterval))
        }
    }

    private func type(_ text: String, source: CGEventSource) {
        let unit = Array(text.utf16)
        var index = 0

        while index < unit.count {
            let end = min(index + Self.chunkSize, unit.count)
            var chunk = Array(unit[index..<end])

            guard let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
                  let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else { return }

            // Unicode payload bypasses keycode translation, so the text lands
            // verbatim regardless of layout — and flags are cleared so a held
            // modifier cannot turn it into a shortcut.
            down.flags = []
            up.flags = []
            down.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: &chunk)
            up.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: &chunk)

            down.post(tap: .cgAnnotatedSessionEventTap)
            up.post(tap: .cgAnnotatedSessionEventTap)
            usleep(useconds_t(Setting.typingInterval))

            index = end
        }
    }
}
