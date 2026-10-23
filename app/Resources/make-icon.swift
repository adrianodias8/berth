// Renders the app icon: a rounded square with a gradient and a ferry glyph.
// Usage: swift make-icon.swift <output.iconset>
import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments[1])
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

func render(_ px: Int) -> Data {
    let size = CGFloat(px)
    let image = NSImage(size: NSSize(width: size, height: size), flipped: false) { rect in
        let inset = rect.insetBy(dx: size * 0.09, dy: size * 0.09)
        let path = NSBezierPath(roundedRect: inset, xRadius: size * 0.2, yRadius: size * 0.2)
        NSGradient(colors: [NSColor(red: 0.07, green: 0.29, blue: 0.62, alpha: 1),
                            NSColor(red: 0.05, green: 0.62, blue: 0.70, alpha: 1)])!
            .draw(in: path, angle: -60)
        let config = NSImage.SymbolConfiguration(pointSize: size * 0.42, weight: .semibold)
            .applying(.init(paletteColors: [.white]))
        if let glyph = NSImage(systemSymbolName: "ferry.fill", accessibilityDescription: nil)?
            .withSymbolConfiguration(config) {
            let g = glyph.size
            glyph.draw(in: NSRect(x: (size - g.width) / 2, y: (size - g.height) / 2 - size * 0.02,
                                  width: g.width, height: g.height))
        }
        return true
    }
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    image.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for base in [16, 32, 128, 256, 512] {
    try render(base).write(to: out.appendingPathComponent("icon_\(base)x\(base).png"))
    try render(base * 2).write(to: out.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
}
