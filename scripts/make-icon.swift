// Draws the 1024x1024 app icon: a rounded square (#1A1D2B to #0B0C12), a soft ion glow and the HubBridge mark
// (two nodes joined by an arc over a baseline) in a mint to ion gradient. Same mark as the logo in
// crates/desktop/src/ui/index.html and android/app/src/main/res/drawable/ic_launcher_foreground.xml (adaptive icon).
// Run: swift scripts/make-icon.swift crates/desktop/assets/icon.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext
let space = CGColorSpaceCreateDeviceRGB()
func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor { CGColor(red: r / 255, green: g / 255, blue: b / 255, alpha: a) }
let mint = rgb(95, 233, 198), ion = rgb(146, 152, 255)

// macOS icon grid: 824 of 1024, corner radius ~22.4 %
let inset: CGFloat = 100
let rect = CGRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let shape = CGPath(roundedRect: rect, cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: rgb(0, 0, 0, 0.35))
ctx.addPath(shape); ctx.setFillColor(rgb(11, 12, 18)); ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(shape); ctx.clip()
let bg = CGGradient(colorsSpace: space, colors: [rgb(26, 29, 43), rgb(11, 12, 18)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(bg, start: CGPoint(x: 0, y: size - inset), end: CGPoint(x: 0, y: inset), options: [])
let glow = CGGradient(colorsSpace: space, colors: [rgb(146, 152, 255, 0.38), rgb(146, 152, 255, 0)] as CFArray, locations: [0, 1])!
ctx.drawRadialGradient(glow, startCenter: CGPoint(x: 512, y: 500), startRadius: 0, endCenter: CGPoint(x: 512, y: 500), endRadius: 420, options: [])
ctx.restoreGState()

// The mark on a 24-unit grid, scale 26, y down (as in the SVG), centered in the icon.
let s: CGFloat = 26
func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: 512 + (x - 12) * s, y: 512 + (13.75 - y) * s) }
func drawGradientClipped(_ build: () -> Void, alpha: CGFloat = 1) {
    ctx.saveGState()
    build()
    ctx.clip()
    ctx.setAlpha(alpha)
    let g = CGGradient(colorsSpace: space, colors: [mint, ion] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(g, start: p(3, 7), end: p(21, 20), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    ctx.restoreGState()
}
// bridge arc (stroke 2.2, round caps) and the two nodes (r 2.2), each clipped on its own so the overlaps stay filled
drawGradientClipped({
    let arc = CGMutablePath()
    arc.move(to: p(5, 16)); arc.addCurve(to: p(19, 16), control1: p(8, 7), control2: p(16, 7))
    ctx.addPath(arc.copy(strokingWithWidth: 2.2 * s, lineCap: .round, lineJoin: .round, miterLimit: 10))
})
for x in [5.0, 19.0] {
    drawGradientClipped({
        let c = p(CGFloat(x), 16)
        ctx.addEllipse(in: CGRect(x: c.x - 2.2 * s, y: c.y - 2.2 * s, width: 4.4 * s, height: 4.4 * s))
    })
}
// baseline (stroke 1.6, 50 % opacity)
drawGradientClipped({
    let line = CGMutablePath()
    line.move(to: p(3, 20)); line.addLine(to: p(21, 20))
    ctx.addPath(line.copy(strokingWithWidth: 1.6 * s, lineCap: .round, lineJoin: .round, miterLimit: 10))
}, alpha: 0.5)

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
