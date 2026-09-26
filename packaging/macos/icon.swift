// Draws Moil's app icon at every size macOS wants, as an .iconset folder
// for `iconutil -c icns`:
//
//   swift icon.swift Moil.iconset
//
// The glyph is the tray icon's chip, on the 36-unit grid crates/moil-app
// draws it on, in white on a blue tile shaped like other macOS app icons.
import AppKit

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: icon.swift Moil.iconset\n".data(using: .utf8)!)
    exit(2)
}
let folder = URL(fileURLWithPath: CommandLine.arguments[1])
try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)

func color(_ hex: UInt32) -> NSColor {
    NSColor(
        srgbRed: CGFloat(hex >> 16 & 0xff) / 255,
        green: CGFloat(hex >> 8 & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: 1)
}

func draw(size: CGFloat) {
    // Apple's grid: an 824-point tile on a 1024-point canvas.
    let unit = size / 1024
    let tile = NSRect(x: 100 * unit, y: 100 * unit, width: 824 * unit, height: 824 * unit)
    let shape = NSBezierPath(roundedRect: tile, xRadius: 185 * unit, yRadius: 185 * unit)

    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowOffset = NSSize(width: 0, height: -10 * unit)
    shadow.shadowBlurRadius = 22 * unit
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.3)
    shadow.set()
    color(0x1f4fd1).setFill()
    shape.fill()
    NSGraphicsContext.restoreGraphicsState()
    NSGradient(starting: color(0x4a86ff), ending: color(0x1c46c4))!.draw(in: shape, angle: -90)

    // The chip, on its 36-unit grid; y runs down on the grid, up in AppKit.
    let glyph = 824 * unit * 0.64
    let origin = (size - glyph) / 2
    let k = glyph / 36
    func rect(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) -> NSRect {
        NSRect(x: origin + x * k, y: origin + (36 - y - h) * k, width: w * k, height: h * k)
    }
    NSColor.white.set()
    let body = NSBezierPath(roundedRect: rect(9, 9, 18, 18), xRadius: 3.5 * k, yRadius: 3.5 * k)
    body.lineWidth = 2.5 * k
    body.stroke()
    for at: CGFloat in [14.5, 21.5] {
        for pin in [
            rect(3.5, at - 1.25, 4.5, 2.5), rect(28, at - 1.25, 4.5, 2.5),
            rect(at - 1.25, 3.5, 2.5, 4.5), rect(at - 1.25, 28, 2.5, 4.5),
        ] {
            NSBezierPath(roundedRect: pin, xRadius: 1.25 * k, yRadius: 1.25 * k).fill()
        }
    }
    NSBezierPath(roundedRect: rect(13.5, 13.5, 9, 9), xRadius: 1.5 * k, yRadius: 1.5 * k).fill()
}

func png(pixels: Int) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: pixels, height: pixels)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw(size: CGFloat(pixels))
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for points in [16, 32, 128, 256, 512] {
    try png(pixels: points).write(to: folder.appendingPathComponent("icon_\(points)x\(points).png"))
    try png(pixels: points * 2).write(to: folder.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
}
