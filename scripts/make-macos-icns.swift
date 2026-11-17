// Builds apps/desktop/assets/icons/synthesis.icns from the owner's transparent logo, following Apple's
// macOS icon template: a squircle body of 896/1024 of the canvas, centred, transparent margin around,
// charcoal gradient fill, the logo drawn at body size (the ribbon spans ~77% of the body width), plus a
// little depth: soft inner edge rim/shade, a faint top sheen under the logo and a drop shadow in the margin.
// Run on macOS from the repo root:  swift scripts/make-macos-icns.swift
// Windows .ico and the Linux PNGs stay full-bleed (those platforms do not pad icons).
import AppKit

let src = "design/01-brand/app-icons/synthesis-logo-transparent-1024.png"
let out = "apps/desktop/assets/icons/synthesis.icns"
let bodyFraction = 896.0 / 1024.0   // Apple template body
let squircleExponent = 5.0          // superellipse |x|^n + |y|^n = 1: continuous corners like Apple's icon shape
let gradientTop: UInt32 = 0x3A3F47
let gradientBottom: UInt32 = 0x14161A
// Depth, in px at 1024 (scaled with the icon). Below depthMinPx the effects would only muddy the icon, so they are skipped.
let depthMinPx = 64
let edgeWidth = 2.0                 // inner rim/shade band along the body edge
let rimAlpha = 0.30                 // white, top edge, fading to 0 by rimFadeEnd of the height
let rimFadeEnd = 0.5
let shadeAlpha = 0.25               // black, bottom edge, from shadeStart of the height down
let shadeStart = 0.55
let sheenAlpha = 0.07               // white, top of the body, fading out by sheenEnd of the height (drawn under the logo)
let sheenEnd = 0.45
let shadowOffset = 10.0, shadowBlur = 22.0, shadowAlpha = 0.38   // must fit in the 64 px margin: 10 + 22 < 64

let set = NSTemporaryDirectory() + "synthesis.iconset"
let fm = FileManager.default
try? fm.removeItem(atPath: set)
try fm.createDirectory(atPath: set, withIntermediateDirectories: true)
guard let art = NSImage(contentsOfFile: src), let logo = art.cgImage(forProposedRect: nil, context: nil, hints: nil) else { fatalError("cannot read \(src)") }
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
func hex(_ h: UInt32) -> CGColor { CGColor(srgbRed: CGFloat((h >> 16) & 255) / 255, green: CGFloat((h >> 8) & 255) / 255, blue: CGFloat(h & 255) / 255, alpha: 1) }
let gradient = CGGradient(colorsSpace: cs, colors: [hex(gradientTop), hex(gradientBottom)] as CFArray, locations: [0, 1])!
func tint(_ white: CGFloat, _ a: Double) -> CGColor { CGColor(srgbRed: white, green: white, blue: white, alpha: a) }
func squircle(_ r: CGRect) -> CGPath {
    let path = CGMutablePath(), steps = 720, e = 2 / squircleExponent
    for i in 0..<steps {
        let t = Double(i) / Double(steps) * 2 * Double.pi, c = cos(t), s = sin(t)
        let dx: Double = (c < 0 ? -1 : 1) * pow(abs(c), e), dy: Double = (s < 0 ? -1 : 1) * pow(abs(s), e)
        let pt = CGPoint(x: Double(r.midX) + Double(r.width) / 2 * dx, y: Double(r.midY) + Double(r.height) / 2 * dy)
        if i == 0 { path.move(to: pt) } else { path.addLine(to: pt) }
    }
    path.closeSubpath()
    return path
}

for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128), ("128x128@2x", 256),
                   ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    let side = (Double(px) * bodyFraction).rounded()
    let off = (Double(px) - side) / 2
    let body = CGRect(x: off, y: off, width: side, height: side)
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
                        space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    let k = Double(px) / 1024, depth = px >= depthMinPx
    let shape = squircle(body)
    if depth {
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -shadowOffset * k), blur: shadowBlur * k, color: tint(0, shadowAlpha))
        ctx.addPath(shape); ctx.setFillColor(hex(gradientBottom)); ctx.fillPath()
        ctx.restoreGState()
    }
    ctx.saveGState()
    ctx.addPath(shape); ctx.clip()
    ctx.drawLinearGradient(gradient, start: CGPoint(x: Double(px) / 2, y: off + side), end: CGPoint(x: Double(px) / 2, y: off), options: [])
    if depth {
        let sheen = CGGradient(colorsSpace: cs, colors: [tint(1, sheenAlpha), tint(1, 0)] as CFArray, locations: [0, 1])!
        ctx.drawLinearGradient(sheen, start: CGPoint(x: Double(px) / 2, y: off + side), end: CGPoint(x: Double(px) / 2, y: off + side * (1 - sheenEnd)), options: [])
    }
    ctx.draw(logo, in: body)
    if depth {  // edge band: the stroke centred on the edge, clipped to the inside by the body clip
        ctx.addPath(shape); ctx.setLineWidth(2 * edgeWidth * k); ctx.replacePathWithStrokedPath(); ctx.clip()
        let edge = CGGradient(colorsSpace: cs, colors: [tint(1, rimAlpha), tint(1, 0), tint(0, 0), tint(0, shadeAlpha)] as CFArray,
                              locations: [0, rimFadeEnd, shadeStart, 1])!
        ctx.drawLinearGradient(edge, start: CGPoint(x: Double(px) / 2, y: off + side), end: CGPoint(x: Double(px) / 2, y: off), options: [])
    }
    ctx.restoreGState()
    let png = NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
    try png.write(to: URL(fileURLWithPath: "\(set)/icon_\(name).png"))
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", out]
try p.run(); p.waitUntilExit()
try? fm.removeItem(atPath: set)
