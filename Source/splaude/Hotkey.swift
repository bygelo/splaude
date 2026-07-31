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
    private(set) static var active: Hotkey?

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

    /// Drops the Carbon grab while keeping the instance registered as active.
    ///
    /// A registered hotkey is consumed before it reaches the responder chain,
    /// so the shortcut recorder can never observe the combo that is currently
    /// bound — pressing it just starts a dictation. Suspending for the duration
    /// of a recording is what makes rebinding the current key possible.
    func suspend() {
        guard let reference else { return }
        UnregisterEventHotKey(reference)
        self.reference = nil
    }

    /// Re-takes a grab dropped by `suspend()`. Safe to call when not suspended.
    @discardableResult
    func resume() -> Bool {
        guard reference == nil, handler != nil else { return reference != nil }
        let identifier = EventHotKeyID(signature: Self.signature, id: 1)
        return RegisterEventHotKey(Setting.hotkeyCode, Setting.hotkeyModifier,
                                   identifier, GetApplicationEventTarget(),
                                   0, &reference) == noErr
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

        return label + name(for: keyCode)
    }

    /// Named keys first, then the live keyboard layout for everything else —
    /// a hard-coded handful meant any other binding rendered as "key 40",
    /// which reads as broken even when the shortcut works.
    private static func name(for keyCode: UInt32) -> String {
        let named: [Int: String] = [
            kVK_Space: "Space", kVK_Return: "Return", kVK_Tab: "Tab",
            kVK_Delete: "Delete", kVK_ForwardDelete: "⌦", kVK_Escape: "Escape",
            kVK_LeftArrow: "←", kVK_RightArrow: "→",
            kVK_UpArrow: "↑", kVK_DownArrow: "↓",
            kVK_Home: "Home", kVK_End: "End",
            kVK_PageUp: "Page Up", kVK_PageDown: "Page Down",
            kVK_F1: "F1", kVK_F2: "F2", kVK_F3: "F3", kVK_F4: "F4",
            kVK_F5: "F5", kVK_F6: "F6", kVK_F7: "F7", kVK_F8: "F8",
            kVK_F9: "F9", kVK_F10: "F10", kVK_F11: "F11", kVK_F12: "F12",
        ]
        if let label = named[Int(keyCode)] { return label }

        guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
              let pointer = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData)
        else { return "key \(keyCode)" }

        let data = Unmanaged<CFData>.fromOpaque(pointer).takeUnretainedValue() as Data
        var deadKey: UInt32 = 0
        var length = 0
        var character = [UniChar](repeating: 0, count: 4)

        let status = data.withUnsafeBytes { raw -> OSStatus in
            guard let layout = raw.baseAddress?.assumingMemoryBound(to: UCKeyboardLayout.self)
            else { return OSStatus(paramErr) }
            return UCKeyTranslate(layout, UInt16(keyCode), UInt16(kUCKeyActionDisplay), 0,
                                  UInt32(LMGetKbdType()), UInt32(kUCKeyTranslateNoDeadKeysBit),
                                  &deadKey, character.count, &length, &character)
        }

        guard status == noErr, length > 0 else { return "key \(keyCode)" }
        return String(utf16CodeUnits: character, count: length).uppercased()
    }
}
