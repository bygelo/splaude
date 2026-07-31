import AppKit

/// A small mic button that floats above every app.
///
/// The critical property is that it never takes keyboard focus. Dictation types
/// into whatever text field is focused, so a button that stole focus on click
/// would type into itself. Hence a non-activating panel that refuses key status.
final class FloatingMic: NSPanel {

    var onToggle: (() -> Void)?

    private let mic = MicView()
    private static let size: CGFloat = 52

    init() {
        super.init(
            contentRect: NSRect(x: 0, y: 0, width: Self.size, height: Self.size),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )

        isFloatingPanel = true
        level = .floating
        backgroundColor = .clear
        isOpaque = false
        hasShadow = true
        hidesOnDeactivate = false
        isMovableByWindowBackground = false
        ignoresMouseEvents = false

        // Follow the user across spaces and sit above full-screen apps, so it is
        // where they left it regardless of what they switched to.
        collectionBehavior = [.canJoinAllSpaces, .stationary, .fullScreenAuxiliary, .ignoresCycle]

        mic.onClick = { [weak self] in self?.onToggle?() }
        mic.onMove = { [weak self] delta in self?.drag(by: delta) }
        mic.onSettled = { [weak self] in
            guard let self else { return }
            Setting.floatingButtonPoint = self.frame.origin
        }

        contentView = mic
        restorePosition()
    }

    /// A borderless panel is not key by default, but be explicit: this must
    /// never pull focus away from the text field being dictated into.
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }

    // MARK: - State

    func set(recording: Bool) {
        mic.isRecording = recording
        mic.needsDisplay = true
    }

    func set(level value: Float) {
        mic.level = value
        mic.needsDisplay = true
    }

    // MARK: - Placement

    private func drag(by delta: CGSize) {
        let origin = NSPoint(x: frame.origin.x + delta.width, y: frame.origin.y + delta.height)
        setFrameOrigin(origin)
    }

    private func restorePosition() {
        if let saved = Setting.floatingButtonPoint, isOnScreen(saved) {
            setFrameOrigin(saved)
            return
        }

        // Default to the lower right, clear of the dock's usual centre.
        guard let screen = NSScreen.main?.visibleFrame else { return }
        setFrameOrigin(NSPoint(x: screen.maxX - Self.size - 32, y: screen.minY + 96))
    }

    /// Guards against restoring onto a display that is no longer attached.
    private func isOnScreen(_ point: CGPoint) -> Bool {
        let rect = NSRect(x: point.x, y: point.y, width: Self.size, height: Self.size)
        return NSScreen.screens.contains { $0.visibleFrame.intersects(rect) }
    }
}

// MARK: - Drawing

private final class MicView: NSView {

    var onClick: (() -> Void)?
    var onMove: ((CGSize) -> Void)?
    var onSettled: (() -> Void)?

    var isRecording = false
    var level: Float = 0

    private var isHovering = false
    private var dragDistance: CGFloat = 0
    private var tracking: NSTrackingArea?

    /// Movement beyond this many points counts as a drag, not a click.
    private static let dragThreshold: CGFloat = 3

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }

        let area = NSTrackingArea(rect: bounds,
                                  options: [.mouseEnteredAndExited, .activeAlways],
                                  owner: self)
        addTrackingArea(area)
        tracking = area
    }

    override func mouseEntered(with event: NSEvent) {
        isHovering = true
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovering = false
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        dragDistance = 0
    }

    override func mouseDragged(with event: NSEvent) {
        dragDistance += abs(event.deltaX) + abs(event.deltaY)
        onMove?(CGSize(width: event.deltaX, height: -event.deltaY))
    }

    override func mouseUp(with event: NSEvent) {
        if dragDistance <= Self.dragThreshold {
            onClick?()
        } else {
            onSettled?()
        }
        dragDistance = 0
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }

        let inset: CGFloat = 6
        let circle = bounds.insetBy(dx: inset, dy: inset)

        // Level ring, drawn outside the button so the glyph stays legible.
        if isRecording && level > 0.02 {
            let spread = inset * CGFloat(min(1, level))
            let ring = circle.insetBy(dx: -spread, dy: -spread)
            context.setFillColor(NSColor.systemRed.withAlphaComponent(0.22).cgColor)
            context.fillEllipse(in: ring)
        }

        let fill: NSColor = isRecording
            ? .systemRed
            : NSColor.controlBackgroundColor.blended(withFraction: isHovering ? 0.10 : 0.0, of: .white) ?? .controlBackgroundColor

        context.setFillColor(fill.cgColor)
        context.fillEllipse(in: circle)

        context.setStrokeColor(NSColor.separatorColor.withAlphaComponent(0.6).cgColor)
        context.setLineWidth(0.5)
        context.strokeEllipse(in: circle.insetBy(dx: 0.25, dy: 0.25))

        let glyphName = isRecording ? "mic.fill" : "mic"
        let configuration = NSImage.SymbolConfiguration(pointSize: 18, weight: .medium)

        guard let glyph = NSImage(systemSymbolName: glyphName, accessibilityDescription: "Dictate")?
            .withSymbolConfiguration(configuration) else { return }

        let tint: NSColor = isRecording ? .white : .labelColor
        let tinted = NSImage(size: glyph.size, flipped: false) { rect in
            glyph.draw(in: rect)
            tint.set()
            rect.fill(using: .sourceAtop)
            return true
        }

        let point = NSPoint(x: bounds.midX - glyph.size.width / 2,
                            y: bounds.midY - glyph.size.height / 2)
        tinted.draw(at: point, from: .zero, operation: .sourceOver, fraction: 1)
    }
}
