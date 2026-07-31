import AppKit
import ApplicationServices

/// Where a take's text belongs, captured when recording starts.
///
/// Synthetic keystrokes land wherever focus happens to be at the moment they
/// are posted, so switching window mid-sentence sprays the rest of a dictation
/// into whatever you switched to — and in live mode that includes backspaces.
/// An anchor pins a take to the field it began in: typing pauses while focus is
/// elsewhere and resumes when it comes back.
///
/// This deliberately does not drag focus around while you are still talking.
/// Yanking a window forward mid-sentence is worse than waiting, so the anchor
/// only reaches for focus at the very end of a take, and only when it has text
/// that would otherwise be lost.
struct FocusAnchor {

    let pid: pid_t
    let appName: String
    private let element: AXUIElement?

    static func capture() -> FocusAnchor? {
        guard let app = NSWorkspace.shared.frontmostApplication else { return nil }
        return FocusAnchor(pid: app.processIdentifier,
                           appName: app.localizedName ?? "unknown",
                           element: focusedElement())
    }

    /// The application the take started in is frontmost again.
    var isAppActive: Bool {
        NSWorkspace.shared.frontmostApplication?.processIdentifier == pid
    }

    /// The same *field* still holds focus, not merely the same app. Moving
    /// between two text areas of one window is still a move, and typing across
    /// it would scatter one sentence over both.
    var holdsFocus: Bool {
        guard isAppActive else { return false }
        // Some apps expose no focused element at all. Those already fall
        // through FocusProbe as permissive, so treat app-level identity as
        // enough rather than refusing to type into them for the whole take.
        guard let element else { return true }
        guard let now = Self.focusedElement() else { return false }
        return CFEqual(element, now)
    }

    /// Writes into the remembered element without disturbing focus, for a take
    /// that ended somewhere else.
    ///
    /// Standard AppKit text fields accept this. Electron, terminals and most
    /// web views do not — hence the Bool rather than a silent best effort.
    func insertDirectly(_ text: String) -> Bool {
        guard let element, !text.isEmpty else { return false }

        var settable = DarwinBoolean(false)
        guard AXUIElementIsAttributeSettable(element,
                                             kAXSelectedTextAttribute as CFString,
                                             &settable) == .success,
              settable.boolValue else { return false }

        // Setting selected text with a collapsed cursor inserts at the caret,
        // which is what a dictation take should do.
        return AXUIElementSetAttributeValue(element,
                                            kAXSelectedTextAttribute as CFString,
                                            text as CFTypeRef) == .success
    }

    /// Brings the anchored app back to the front so a paste lands where the
    /// take started. A last resort — see the note on this type.
    @discardableResult
    func reactivate() -> Bool {
        NSRunningApplication(processIdentifier: pid)?.activate() ?? false
    }

    private static func focusedElement() -> AXUIElement? {
        let system = AXUIElementCreateSystemWide()
        var focused: CFTypeRef?
        guard AXUIElementCopyAttributeValue(system,
                                            kAXFocusedUIElementAttribute as CFString,
                                            &focused) == .success,
              let focused else { return nil }
        // swiftlint:disable:next force_cast
        return (focused as! AXUIElement)
    }
}
