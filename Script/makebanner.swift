// Renders the splaude social/README banner at GitHub's 1280x640 preview size.
//
//   swift makebanner.swift <hex> <output.png>

import AppKit

let argument = CommandLine.arguments
guard argument.count == 3 else {
    FileHandle.standardError.write("usage: makebanner.swift <hex> <output.png>\n".data(using: .utf8)!)
    exit(1)
}

let hex = argument[1].hasPrefix("#") ? String(argument[1].dropFirst()) : argument[1]
let outputPath = argument[2]

func color(_ hex: String, brightness: CGFloat = 1.0) -> NSColor {
    var value: UInt64 = 0
    Scanner(string: hex).scanHexInt64(&value)
    return NSColor(srgbRed: min(CGFloat((value >> 16) & 0xFF) / 255 * brightness, 1),
                   green: min(CGFloat((value >> 8) & 0xFF) / 255 * brightness, 1),
                   blue: min(CGFloat(value & 0xFF) / 255 * brightness, 1),
                   alpha: 1)
}

let width: CGFloat = 1280, height: CGFloat = 640
let scale: CGFloat = 2

let rep = NSBitmapImageRep(bitmapDataPlanes: nil,
                           pixelsWide: Int(width * scale), pixelsHigh: Int(height * scale),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                           isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
rep.size = NSSize(width: width, height: height)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let context = NSGraphicsContext.current!.cgContext

// Warm near-black rather than neutral grey: the tint is a clay orange, and a
// cool ground makes it read muddy.
color("17120E").setFill()
NSRect(x: 0, y: 0, width: width, height: height).fill()

// A wide, very soft glow behind the mark, so the panel does not look like flat
// paper without introducing a visible gradient band.
context.saveGState()
let glow = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                      colors: [color(hex).withAlphaComponent(0.20).cgColor,
                               color(hex).withAlphaComponent(0.0).cgColor] as CFArray,
                      locations: [0, 1])!
context.drawRadialGradient(glow,
                           startCenter: CGPoint(x: width / 2, y: height * 0.60), startRadius: 0,
                           endCenter: CGPoint(x: width / 2, y: height * 0.60), endRadius: width * 0.46,
                           options: [])
context.restoreGState()

// MARK: - Mark

let side: CGFloat = 200
let squircle = CGRect(x: (width - side) / 2, y: height * 0.545, width: side, height: side)
let radius = side * (185.0 / 824.0)
let path = CGPath(roundedRect: squircle, cornerWidth: radius, cornerHeight: radius, transform: nil)

context.saveGState()
context.addPath(path)
context.clip()
let face = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                      colors: [color(hex, brightness: 1.12).cgColor, color(hex).cgColor] as CFArray,
                      locations: [0, 1])!
context.drawLinearGradient(face,
                           start: CGPoint(x: 0, y: squircle.maxY),
                           end: CGPoint(x: 0, y: squircle.minY), options: [])
context.restoreGState()

let configuration = NSImage.SymbolConfiguration(pointSize: side * 0.52, weight: .medium)
if let symbol = NSImage(systemSymbolName: "mic.fill", accessibilityDescription: nil)?
    .withSymbolConfiguration(configuration) {
    let drawn = symbol.size
    let white = NSImage(size: drawn)
    white.lockFocus()
    symbol.draw(in: NSRect(origin: .zero, size: drawn))
    NSColor.white.set()
    NSRect(origin: .zero, size: drawn).fill(using: .sourceAtop)
    white.unlockFocus()
    white.draw(in: NSRect(x: squircle.midX - drawn.width / 2,
                          y: squircle.midY - drawn.height / 2 - side * 0.005,
                          width: drawn.width, height: drawn.height))
}

// MARK: - Type

func draw(_ text: String, font: NSFont, color: NSColor, centreY: CGFloat, tracking: CGFloat = 0) {
    let attribute: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: color, .kern: tracking,
    ]
    let size = text.size(withAttributes: attribute)
    text.draw(at: NSPoint(x: (width - size.width) / 2, y: centreY - size.height / 2),
              withAttributes: attribute)
}

// The rounded system face echoes the squircle; the default grotesque fights it.
func system(_ size: CGFloat, _ weight: NSFont.Weight) -> NSFont {
    let base = NSFont.systemFont(ofSize: size, weight: weight)
    guard let descriptor = base.fontDescriptor.withDesign(.rounded) else { return base }
    return NSFont(descriptor: descriptor, size: size) ?? base
}

draw("splaude", font: system(104, .bold), color: .white, centreY: height * 0.395, tracking: -2.5)
draw("Push-to-talk dictation for macOS",
     font: system(34, .medium),
     color: NSColor(white: 1, alpha: 0.62), centreY: height * 0.275)
draw("HOLD ⌥SPACE · TALK · IT TYPES WHERE YOUR CURSOR IS",
     font: system(19, .semibold),
     color: color(hex, brightness: 1.25).withAlphaComponent(0.95),
     centreY: height * 0.145, tracking: 2.4)

NSGraphicsContext.restoreGraphicsState()

try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: outputPath))
print("wrote \(outputPath)")
