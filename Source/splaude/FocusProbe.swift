import AppKit
import ApplicationServices

/// Asks the accessibility API what currently owns the keyboard focus, so live
/// typing only ever fires into something that can accept text.
///
/// This is a safety gate, not a nicety: live typing emits backspaces, and
/// backspaces sent into a file browser or a canvas are somebody else's bad day.
enum FocusProbe {

    enum Target {
        /// A text field, text area, search field or similar.
        case editable(String)
        /// Focus resolved to something that clearly cannot take text.
        case notEditable(String)
        /// The app exposes nothing useful. Many apps have poor accessibility
        /// support, so this is treated as permissive.
        case unknown

        var acceptsTyping: Bool {
            switch self {
            case .editable, .unknown: return true
            case .notEditable: return false
            }
        }

        var label: String {
            switch self {
            case .editable(let role): return role
            case .notEditable(let role): return "\(role) (not editable)"
            case .unknown: return "unknown"
            }
        }
    }

    private static let editableRole: Set<String> = [
        kAXTextFieldRole, kAXTextAreaRole, kAXComboBoxRole,
    ]

    /// Roles that are definitely not text entry, where a stray backspace could
    /// do real damage.
    private static let hostileRole: Set<String> = [
        kAXOutlineRole, kAXBrowserRole, kAXTableRole, kAXImageRole, kAXButtonRole,
    ]

    static func current() -> Target {
        let system = AXUIElementCreateSystemWide()

        var focused: CFTypeRef?
        let status = AXUIElementCopyAttributeValue(system, kAXFocusedUIElementAttribute as CFString, &focused)

        guard status == .success, let element = focused else { return .unknown }
        // swiftlint:disable:next force_cast
        let target = element as! AXUIElement

        var roleValue: CFTypeRef?
        AXUIElementCopyAttributeValue(target, kAXRoleAttribute as CFString, &roleValue)
        let role = (roleValue as? String) ?? "unknown"

        if editableRole.contains(role) { return .editable(role) }

        // A settable value attribute is the most reliable signal for the many
        // custom text views that report an unhelpful role — editors, terminals,
        // and web content especially.
        var settable = DarwinBoolean(false)
        if AXUIElementIsAttributeSettable(target, kAXValueAttribute as CFString, &settable) == .success,
           settable.boolValue {
            return .editable(role)
        }

        // Web areas and editor surfaces frequently report a container role while
        // still handling keystrokes. Only refuse the roles that are unambiguous.
        if hostileRole.contains(role) { return .notEditable(role) }

        return .unknown
    }

    static var frontmostApp: String {
        NSWorkspace.shared.frontmostApplication?.localizedName ?? "unknown"
    }
}
