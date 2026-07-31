import AppKit
import Carbon.HIToolbox

/// A system-wide hotkey registered through Carbon, which is still the only API
/// that *consumes* the keystroke — a global NSEvent monitor would let the combo
/// leak into whatever app is focused.
///
/// Behaviour is press-and-hold *or* tap-to-toggle, decided by how long the key
/// is held: a quick tap latches recording on, holding dictates until release.
final class Hotkey {

    var onEngage: (() -> Void)?
    var onRelease: ((_ wasHold: Bool) -> Void)?

    private var reference: EventHotKeyRef?
    private var handler: EventHandlerRef?
    private var pressedAt: Date?

    /// Below this, the press counts as a tap rather than a hold.
    private static let holdThreshold: TimeInterval = 0.4

    private static let signature = OSType(0x53504C44)  // 'SPLD'
    private static var active: Hotkey?

    func register(keyCode: UInt32 = Setting.hotkeyCode,
                  modifier: UInt32 = Setting.hotkeyModifier) -> Bool {
        unregister()
        Self.active = self

        var eventType = [
            EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed)),
            EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyReleased)),
        ]

        let installed = InstallEventHandler(
            GetApplicationEventTarget(),
            { _, event, _ -> OSStatus in
                Hotkey.active?.dispatch(event)
                return noErr
            },
            eventType.count,
            &eventType,
            nil,
            &handler
        )

        guard installed == noErr else { return false }

        let identifier = EventHotKeyID(signature: Self.signature, id: 1)
        let registered = RegisterEventHotKey(keyCode, modifier, identifier,
                                             GetApplicationEventTarget(), 0, &reference)

        return registered == noErr
    }

    func unregister() {
        if let reference {
            UnregisterEventHotKey(reference)
            self.reference = nil
        }
        if let handler {
            RemoveEventHandler(handler)
            self.handler = nil
        }
        if Self.active === self { Self.active = nil }
    }

    deinit { unregister() }

    // MARK: - Dispatch

    private func dispatch(_ event: EventRef?) {
        guard let event else { return }

        switch Int(GetEventKind(event)) {
        case kEventHotKeyPressed:
            pressedAt = Date()
            onEngage?()

        case kEventHotKeyReleased:
            let held = pressedAt.map { Date().timeIntervalSince($0) } ?? 0
            pressedAt = nil
            onRelease?(held >= Self.holdThreshold)

        default:
            break
        }
    }

    /// Human-readable form of the configured combo, for the menu.
    static func describe(keyCode: UInt32 = Setting.hotkeyCode,
                         modifier: UInt32 = Setting.hotkeyModifier) -> String {
        var label = ""
        if modifier & UInt32(controlKey) != 0 { label += "⌃" }
        if modifier & UInt32(optionKey) != 0 { label += "⌥" }
        if modifier & UInt32(shiftKey) != 0 { label += "⇧" }
        if modifier & UInt32(cmdKey) != 0 { label += "⌘" }

        switch Int(keyCode) {
        case kVK_Space: label += "Space"
        case kVK_ANSI_D: label += "D"
        case kVK_ANSI_V: label += "V"
        case kVK_Return: label += "Return"
        default: label += "key \(keyCode)"
        }

        return label
    }
}
