// Regenerates the app icons (macOS only: CoreGraphics + iconutil). Run from the repo root: swift scripts/make-icons.swift
// Art: the transparent brand symbol on a monochrome graphite gradient (no pure black).
// Writes design/01-brand/app-icons/* (full-bleed rounded square, Linux/Windows) and apps/desktop/assets/icons/* (.icns uses the Apple grid: 824 body in 1024).
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let cs = CGColorSpace(name: CGColorSpace.sRGB)!
func rgb(_ r: Int, _ g: Int, _ b: Int, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [CGFloat(r) / 255, CGFloat(g) / 255, CGFloat(b) / 255, a])!
}
func load(_ p: String) -> CGImage {
    CGImageSourceCreateImageAtIndex(CGImageSourceCreateWithURL(URL(fileURLWithPath: p) as CFURL, nil)!, 0, nil)!
}
func ctx(_ n: Int) -> CGContext {
    let c = CGContext(data: nil, width: n, height: n, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.interpolationQuality = .high
    return c
}
func png(_ img: CGImage) -> Data {
    let d = NSMutableData()
    let dest = CGImageDestinationCreateWithData(d, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, img, nil)
    CGImageDestinationFinalize(dest)
    return d as Data
}
func glow(_ c: CGContext, _ x: CGFloat, _ y: CGFloat, _ r: CGFloat, _ col: (Int, Int, Int), _ a: CGFloat) {
    let g = CGGradient(colorsSpace: cs, colors: [rgb(col.0, col.1, col.2, a), rgb(col.0, col.1, col.2, 0)] as CFArray, locations: [0, 1])!
    c.drawRadialGradient(g, startCenter: CGPoint(x: x, y: y), startRadius: 0, endCenter: CGPoint(x: x, y: y), endRadius: r, options: [])
}

// Palette: monochrome graphite, no pure black. Change the look here.
let baseTop = (74, 74, 77), baseBottom = (27, 27, 30)  // top-left warm grey to bottom-right charcoal
let bloomAlpha: CGFloat = 0.16                          // neutral white bloom behind the symbol
let glyphLift: CGFloat = 0.42                           // white veil over the symbol: frosted silver
let specularAlpha: CGFloat = 0.55                       // bright specular on the upper-left of the symbol

/// 1024 master; `body` is the side of the rounded square, centred (1024 = full bleed).
func master(body: CGFloat) -> CGImage {
    let c = ctx(1024)
    let o = (1024 - body) / 2
    let rect = CGRect(x: o, y: o, width: body, height: body)
    let path = CGPath(roundedRect: rect, cornerWidth: body * 0.2237, cornerHeight: body * 0.2237, transform: nil)
    c.saveGState()
    c.addPath(path); c.clip()
    // CG origin is bottom-left: start top-left, end bottom-right
    let base = CGGradient(colorsSpace: cs, colors: [rgb(baseTop.0, baseTop.1, baseTop.2), rgb(baseBottom.0, baseBottom.1, baseBottom.2)] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(base, start: CGPoint(x: o, y: o + body), end: CGPoint(x: o + body * 0.8, y: o + body * 0.1), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    glow(c, 512, 512, body * 0.5, (255, 255, 255), bloomAlpha)
    // soft inner shadow at the bottom
    let sh = CGGradient(colorsSpace: cs, colors: [rgb(0, 0, 0, 0), rgb(0, 0, 0, 0.14)] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(sh, start: CGPoint(x: 0, y: o + body * 0.14), end: CGPoint(x: 0, y: o), options: [])
    // symbol on its own layer so the lift and specular only touch the symbol; soft shadow below it
    let s = body * 0.80
    let g = ctx(1024)
    g.draw(load("design/01-brand/logo/png/synthesis-symbol-transparent-1024.png"),
           in: CGRect(x: 512 - s / 2, y: 512 - s / 2, width: s, height: s))
    g.setBlendMode(.sourceAtop)
    g.setFillColor(rgb(255, 255, 255, glyphLift)); g.fill(CGRect(x: 0, y: 0, width: 1024, height: 1024))
    let spec = CGGradient(colorsSpace: cs, colors: [rgb(255, 255, 255, specularAlpha), rgb(255, 255, 255, 0)] as CFArray, locations: [0, 1])!
    g.drawLinearGradient(spec, start: CGPoint(x: 512 - s * 0.45, y: 512 + s * 0.45), end: CGPoint(x: 512, y: 512), options: [])
    c.setShadow(offset: CGSize(width: 0, height: -body * 0.025), blur: body * 0.04, color: rgb(0, 0, 0, 0.35))
    c.draw(g.makeImage()!, in: CGRect(x: 0, y: 0, width: 1024, height: 1024))
    c.setShadow(offset: .zero, blur: 0)
    c.restoreGState()
    // thin rim, lighter at the top
    c.saveGState()
    c.addPath(path); c.clip()
    c.addPath(path); c.setLineWidth(body * 0.008)
    c.replacePathWithStrokedPath(); c.clip()
    let hi = CGGradient(colorsSpace: cs, colors: [rgb(255, 255, 255, 0.30), rgb(255, 255, 255, 0.05)] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(hi, start: CGPoint(x: 0, y: o + body), end: CGPoint(x: 0, y: o), options: [])
    c.restoreGState()
    return c.makeImage()!
}
func scaled(_ m: CGImage, _ n: Int) -> CGImage {
    if n == 1024 { return m }
    let c = ctx(n)
    c.draw(m, in: CGRect(x: 0, y: 0, width: n, height: n))
    return c.makeImage()!
}
func write(_ d: Data, _ p: String) { try! d.write(to: URL(fileURLWithPath: p)) }

let full = master(body: 1024), mac = master(body: 824)
let brand = "design/01-brand/app-icons", assets = "apps/desktop/assets/icons"
for n in [16, 32, 64, 128, 256, 512, 1024] { write(png(scaled(full, n)), "\(brand)/synthesis-app-icon-\(n).png") }
write(png(scaled(full, 256)), "\(assets)/synthesis-256.png")
write(png(scaled(full, 512)), "\(assets)/synthesis-512.png")

// .ico with PNG-compressed entries
let sizes = [16, 24, 32, 48, 64, 128, 256]
let imgs = sizes.map { png(scaled(full, $0)) }
var ico = Data([0, 0, 1, 0, UInt8(sizes.count), 0])
var off = 6 + 16 * sizes.count
func le<T: FixedWidthInteger>(_ v: T) -> [UInt8] { withUnsafeBytes(of: v.littleEndian) { Array($0) } }
for (i, n) in sizes.enumerated() {
    ico.append(contentsOf: [UInt8(n % 256), UInt8(n % 256), 0, 0] + le(UInt16(1)) + le(UInt16(32)) + le(UInt32(imgs[i].count)) + le(UInt32(off)))
    off += imgs[i].count
}
imgs.forEach { ico.append($0) }
write(ico, "\(assets)/synthesis.ico")
write(ico, "\(brand)/synthesis-app-icon.ico")

// .icns via iconutil
let set = NSTemporaryDirectory() + "synthesis.iconset"
try? FileManager.default.removeItem(atPath: set)
try! FileManager.default.createDirectory(atPath: set, withIntermediateDirectories: true)
for n in [16, 32, 128, 256, 512] {
    write(png(scaled(mac, n)), "\(set)/icon_\(n)x\(n).png")
    write(png(scaled(mac, n * 2)), "\(set)/icon_\(n)x\(n)@2x.png")
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", "\(assets)/synthesis.icns"]
try! p.run(); p.waitUntilExit()
precondition(p.terminationStatus == 0, "iconutil failed")
