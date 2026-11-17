// Builds apps/desktop/assets/icons/synthesis.icns from the full-bleed brand PNG, following Apple's
// macOS icon template: the artwork is 896/1024 of the canvas, centred, transparent margin around.
// Run on macOS from the repo root:  swift scripts/make-macos-icns.swift
// The ribbon is enlarged by ribbonScale about the centre inside the squircle; the squircle mask (the
// source's own alpha) is re-applied at the body size so the transparent corners do not move.
// Windows .ico and the Linux PNGs stay full-bleed (those platforms do not pad icons).
import AppKit

let ribbonScale = 1.25  // source ribbon spans ~62% of the body; x1.25 gives ~77%
let src = "design/01-brand/app-icons/synthesis-app-icon-1024.png"
let out = "apps/desktop/assets/icons/synthesis.icns"
let set = NSTemporaryDirectory() + "synthesis.iconset"
let fm = FileManager.default
try? fm.removeItem(atPath: set)
try fm.createDirectory(atPath: set, withIntermediateDirectories: true)
guard let art = NSImage(contentsOfFile: src), let cg = art.cgImage(forProposedRect: nil, context: nil, hints: nil) else { fatalError("cannot read \(src)") }

for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128), ("128x128@2x", 256),
                   ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    let side = (Double(px) * 896 / 1024).rounded()
    let off = (Double(px) - side) / 2
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    let big = side * ribbonScale
    ctx.clip(to: CGRect(x: off, y: off, width: side, height: side))  // the enlarged art must not spill into the margin
    ctx.draw(cg, in: CGRect(x: off + (side - big) / 2, y: off + (side - big) / 2, width: big, height: big))
    ctx.setBlendMode(.destinationIn)  // keep only where the unscaled squircle is opaque
    ctx.draw(cg, in: CGRect(x: off, y: off, width: side, height: side))
    let png = NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
    try png.write(to: URL(fileURLWithPath: "\(set)/icon_\(name).png"))
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", out]
try p.run(); p.waitUntilExit()
try? fm.removeItem(atPath: set)
exit(p.terminationStatus)
