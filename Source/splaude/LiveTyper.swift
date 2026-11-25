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

    /// Set from the main thread the moment the screen stops matching `typed` — the
    /// message was sent (Return cleared the field) or the user typed into it. Every
    /// queued update and every key still to be posted checks it, so nothing typed
    /// after that point lands in the emptied box or backspaces over the user's text.
    private let gate = NSLock()
    private var halted = false
    /// Bumped by `reset()`: an update queued in an earlier take never runs in the next one.
    private var take = 0
    private var isHalted: Bool { gate.lock(); defer { gate.unlock() }; return halted }
    private func isLive(_ t: Int) -> Bool { gate.lock(); defer { gate.unlock() }; return !halted && take == t }

    /// Tags splaude's own synthetic keys, so a keyboard monitor can tell them from the user's.
    static let mark: Int64 = 0x73706c64  // "spld"

    /// Longest run of backspaces to accept before giving up and leaving the
    /// text alone — a runaway diff must never machine-gun the delete key.
    private static let maxRewrite = 240
    /// UTF-16 units per synthetic event.
    private static let chunkSize = 16

    var isEmpty: Bool { typed.isEmpty }
    var text: String { typed }
    /// `text`, read on the typer's own queue so a burst in flight cannot tear it.
    func snapshot() -> String { queue.sync { typed } }

    /// Stops typing now, mid-burst included. Lasts until `reset()`.
    func halt() {
        gate.lock(); halted = true; gate.unlock()
    }

    func reset() {
        gate.lock(); halted = false; take += 1; gate.unlock()
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
        gate.lock(); let t = take; gate.unlock()
        queue.async { [weak self] in
            guard let self, self.isLive(t) else { return }

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

            // Record what actually reached the screen, not what was intended: a halt
            // mid-burst leaves part of it unposted, and the next segment counts from here.
            let deleted = removeCount > 0 ? self.backspace(removeCount, source: source) : 0
            guard deleted == removeCount else {
                self.typed = String(current[0..<(current.count - deleted)])
                return
            }
            let base = String(desired[0..<min(boundary, desired.count)])
            let posted = addition.isEmpty ? 0 : self.type(addition, source: source)
            let unit = Array(addition.utf16)
            self.typed = base + (posted >= unit.count
                ? addition
                : String(decoding: unit[0..<posted], as: UTF16.self))
        }
    }

    // MARK: - Synthetic keys

    /// Returns how many backspaces were posted before a halt.
    private func backspace(_ count: Int, source: CGEventSource) -> Int {
        let delete: CGKeyCode = 51  // kVK_Delete

        for done in 0..<count {
            guard !isHalted else { return done }
            guard let down = CGEvent(keyboardEventSource: source, virtualKey: delete, keyDown: true),
                  let up = CGEvent(keyboardEventSource: source, virtualKey: delete, keyDown: false) else { return done }

            // Push-to-talk means Option is very likely physically held right
            // now, and Option-Delete deletes a whole word. Clear the hardware
            // modifier state off every synthetic event.
            down.flags = []
            up.flags = []

            down.setIntegerValueField(.eventSourceUserData, value: Self.mark)
            up.setIntegerValueField(.eventSourceUserData, value: Self.mark)
            down.post(tap: .cgAnnotatedSessionEventTap)
            up.post(tap: .cgAnnotatedSessionEventTap)
            usleep(useconds_t(Setting.typingInterval))
        }
        return count
    }

    /// Returns how many UTF-16 units were posted before a halt.
    private func type(_ text: String, source: CGEventSource) -> Int {
        let unit = Array(text.utf16)
        var index = 0

        while index < unit.count {
            guard !isHalted else { return index }
            let end = min(index + Self.chunkSize, unit.count)
            var chunk = Array(unit[index..<end])

            guard let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
                  let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else { return index }

            // Unicode payload bypasses keycode translation, so the text lands
            // verbatim regardless of layout — and flags are cleared so a held
            // modifier cannot turn it into a shortcut.
            down.flags = []
            up.flags = []
            down.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: &chunk)
            up.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: &chunk)

            down.setIntegerValueField(.eventSourceUserData, value: Self.mark)
            up.setIntegerValueField(.eventSourceUserData, value: Self.mark)
            down.post(tap: .cgAnnotatedSessionEventTap)
            up.post(tap: .cgAnnotatedSessionEventTap)
            usleep(useconds_t(Setting.typingInterval))

            index = end
        }
        return index
    }
}
