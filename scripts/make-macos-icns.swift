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
let cornerFraction = 0.225          // corner radius / body
let cornerSmoothing = 0.6           // Figma-style corner smoothing (0 = circular arc): G2-continuous blend into the straight edges
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
// Rounded rect with smoothed (continuous) corners, after the Figma corner-smoothing construction: each corner is two
// cubic Beziers plus a small circular arc, starting (1 + smoothing) * radius from the corner; the edges stay straight.
func squircle(_ r: CGRect) -> CGPath {
    let rad = Double(r.width) * cornerFraction, sm = cornerSmoothing
    let p = (1 + sm) * rad
    let theta = (.pi / 2) * (1 - sm)                      // angle of the circular arc
    let arcLen = sin(theta / 2) * rad * 2.0.squareRoot()  // chord of that arc
    let beta = (.pi / 4) * sm
    let c = rad * tan((.pi / 2 - theta) / 4) * cos(beta), d = c * tan(beta)
    let b = (p - arcLen - c - d) / 3, a = 2 * b
    let k = 4.0 / 3.0 * tan(theta / 4) * rad              // cubic control length for the arc
    let t0 = (x: -cos(beta), y: sin(beta)), t1 = (x: -sin(beta), y: cos(beta))
    // corner in a local frame: origin at the corner, x along the incoming edge (away from the corner), y inward
    let p0 = (x: p - a - b - c, y: d), p3 = (x: p - a - b - c - arcLen, y: d + arcLen)
    let local: [(Double, Double)] = [(p - a, 0), (p - a - b, 0), (p0.x, p0.y),
        (p0.x + k * t0.x, p0.y + k * t0.y), (p3.x - k * t1.x, p3.y - k * t1.y), (p3.x, p3.y),
        (p3.x - d, p3.y + c), (p3.x - d, p3.y + b + c), (0, p)]
    let (x0, x1, y0, y1) = (Double(r.minX), Double(r.maxX), Double(r.minY), Double(r.maxY))
    let maps: [((Double, Double)) -> CGPoint] = [   // clockwise from the top-right corner
        { CGPoint(x: x1 - $0.0, y: y1 - $0.1) }, { CGPoint(x: x1 - $0.1, y: y0 + $0.0) },
        { CGPoint(x: x0 + $0.0, y: y0 + $0.1) }, { CGPoint(x: x0 + $0.1, y: y1 - $0.0) }]
    let path = CGMutablePath()
    for (i, m) in maps.enumerated() {
        let start = m((p, 0))
        if i == 0 { path.move(to: start) } else { path.addLine(to: start) }
        for j in stride(from: 0, to: local.count, by: 3) { path.addCurve(to: m(local[j + 2]), control1: m(local[j]), control2: m(local[j + 1])) }
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
