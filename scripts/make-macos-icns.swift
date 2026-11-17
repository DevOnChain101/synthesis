// Builds apps/desktop/assets/icons/synthesis.icns from the owner's transparent logo, following Apple's
// macOS icon template: a squircle body of 896/1024 of the canvas, centred, transparent margin around,
// charcoal gradient fill, the logo drawn at body size (the ribbon spans ~77% of the body width).
// Run on macOS from the repo root:  swift scripts/make-macos-icns.swift
// Windows .ico and the Linux PNGs stay full-bleed (those platforms do not pad icons).
import AppKit

let src = "design/01-brand/app-icons/synthesis-logo-transparent-512.png"
let out = "apps/desktop/assets/icons/synthesis.icns"
let bodyFraction = 896.0 / 1024.0   // Apple template body
let cornerFraction = 0.2237         // corner radius / body, Apple's continuous corner approximated by a circular arc
let gradientTop: UInt32 = 0x3A3F47
let gradientBottom: UInt32 = 0x14161A

let set = NSTemporaryDirectory() + "synthesis.iconset"
let fm = FileManager.default
try? fm.removeItem(atPath: set)
try fm.createDirectory(atPath: set, withIntermediateDirectories: true)
guard let art = NSImage(contentsOfFile: src), let logo = art.cgImage(forProposedRect: nil, context: nil, hints: nil) else { fatalError("cannot read \(src)") }
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
func hex(_ h: UInt32) -> CGColor { CGColor(srgbRed: CGFloat((h >> 16) & 255) / 255, green: CGFloat((h >> 8) & 255) / 255, blue: CGFloat(h & 255) / 255, alpha: 1) }
let gradient = CGGradient(colorsSpace: cs, colors: [hex(gradientTop), hex(gradientBottom)] as CFArray, locations: [0, 1])!

for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128), ("128x128@2x", 256),
                   ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    let side = (Double(px) * bodyFraction).rounded()
    let off = (Double(px) - side) / 2
    let body = CGRect(x: off, y: off, width: side, height: side)
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
                        space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    ctx.addPath(CGPath(roundedRect: body, cornerWidth: side * cornerFraction, cornerHeight: side * cornerFraction, transform: nil))
    ctx.clip()
    ctx.drawLinearGradient(gradient, start: CGPoint(x: Double(px) / 2, y: off + side), end: CGPoint(x: Double(px) / 2, y: off), options: [])
    ctx.draw(logo, in: body)
    let png = NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
    try png.write(to: URL(fileURLWithPath: "\(set)/icon_\(name).png"))
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", out]
try p.run(); p.waitUntilExit()
try? fm.removeItem(atPath: set)
