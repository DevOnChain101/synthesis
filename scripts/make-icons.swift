// Regenerates the app icons (macOS only: CoreGraphics + iconutil). Run from the repo root: swift scripts/make-icons.swift
// Art: the owner's icon image (design/01-brand/app-icons/source/owner-icon-master.jpg), cropped, denoised, upscaled and sharpened, then cut to the macOS squircle.
// Writes design/01-brand/app-icons/* (full-bleed rounded square, Linux/Windows) and apps/desktop/assets/icons/* (.icns uses the Apple grid: 824 body in 1024).
import CoreGraphics
import CoreImage
import Foundation
import ImageIO
import UniformTypeIdentifiers

let cs = CGColorSpace(name: CGColorSpace.sRGB)!
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

// Master: the owner's icon (294x296 JPEG). Its body fills the image, with the surround only at the corners; the crop
// below is inset past the old rounded edge and its rim so the new mask is clean. Colours are never altered.
let masterPath = "design/01-brand/app-icons/source/owner-icon-master.jpg"
let crop = CGRect(x: 4, y: 5, width: 286, height: 286)  // x, y from the top-left of the JPEG
let ci = CIContext(options: [.workingColorSpace: cs, .outputColorSpace: cs])

func resample(_ img: CIImage, to n: Int, sharpen radius: Double, _ intensity: Double) -> CIImage {
    let lan = CIFilter(name: "CILanczosScaleTransform")!
    lan.setValue(img, forKey: kCIInputImageKey)
    lan.setValue(CGFloat(n) / img.extent.width, forKey: kCIInputScaleKey)
    lan.setValue(1.0, forKey: kCIInputAspectRatioKey)
    let up = lan.outputImage!.cropped(to: CGRect(x: 0, y: 0, width: n, height: n))
    let u = CIFilter(name: "CIUnsharpMask")!
    u.setValue(up, forKey: kCIInputImageKey)
    u.setValue(radius, forKey: kCIInputRadiusKey)
    u.setValue(intensity, forKey: kCIInputIntensityKey)
    return u.outputImage!.cropped(to: up.extent)
}
func render(_ img: CIImage) -> CGImage { ci.createCGImage(img, from: img.extent, format: .RGBA8, colorSpace: cs)! }

/// Light JPEG-block denoise, then a 1024 Lanczos upscale and a mild unsharp mask (radius 2.5 px, intensity 0.45).
let master1024: CGImage = {
    let src = load(masterPath).cropping(to: crop)!
    let dn = CIFilter(name: "CINoiseReduction")!
    dn.setValue(CIImage(cgImage: src), forKey: kCIInputImageKey)
    dn.setValue(0.01, forKey: "inputNoiseLevel")
    dn.setValue(0.4, forKey: kCIInputSharpnessKey)
    let clean = dn.outputImage!.cropped(to: CIImage(cgImage: src).extent)
    return render(resample(clean, to: 1024, sharpen: 2.5, 0.45))
}()

/// Apple-style continuous-corner rounded square filling `rect` (radius 22.37% of the side; the standard "iOS 7" bezier construction).
func squircle(_ rect: CGRect) -> CGPath {
    let r = rect.width * 0.2237, p = CGMutablePath()
    // (corner, direction along the incoming edge, direction along the outgoing edge), clockwise
    let corners: [(CGPoint, CGPoint, CGPoint)] = [
        (CGPoint(x: rect.maxX, y: rect.minY), CGPoint(x: 1, y: 0), CGPoint(x: 0, y: 1)),
        (CGPoint(x: rect.maxX, y: rect.maxY), CGPoint(x: 0, y: 1), CGPoint(x: -1, y: 0)),
        (CGPoint(x: rect.minX, y: rect.maxY), CGPoint(x: -1, y: 0), CGPoint(x: 0, y: -1)),
        (CGPoint(x: rect.minX, y: rect.minY), CGPoint(x: 0, y: -1), CGPoint(x: 1, y: 0)),
    ]
    for (i, (c, e1, e2)) in corners.enumerated() {
        func q(_ a: CGFloat, _ b: CGFloat) -> CGPoint {
            CGPoint(x: c.x - a * r * e1.x + b * r * e2.x, y: c.y - a * r * e1.y + b * r * e2.y)
        }
        i == 0 ? p.move(to: q(1.52866483, 0)) : p.addLine(to: q(1.52866483, 0))
        p.addCurve(to: q(0.63149379, 0.07491139), control1: q(1.08849296, 0), control2: q(0.86840694, 0))
        p.addCurve(to: q(0.07491139, 0.63149379), control1: q(0.37282383, 0.16905956), control2: q(0.16905956, 0.37282383))
        p.addCurve(to: q(0, 1.52866483), control1: q(0, 0.86840694), control2: q(0, 1.08849296))
    }
    p.closeSubpath()
    return p
}

/// n px canvas with the squircle body at `fraction` of it (1.0 = full bleed, 824/1024 = Apple grid); no shadow.
/// Each size is resampled from the master and lightly sharpened before the mask, so the mask edge gets no halo.
func icon(_ n: Int, fraction: CGFloat) -> CGImage {
    let b = (CGFloat(n) * fraction).rounded()
    let body = n == 1024 && fraction == 1 ? master1024
        : render(resample(CIImage(cgImage: master1024), to: Int(b), sharpen: 0.8, b <= 256 ? 0.35 : 0.15))
    let c = ctx(n)
    let o = (CGFloat(n) - b) / 2
    let rect = CGRect(x: o, y: o, width: b, height: b)
    c.addPath(squircle(rect)); c.clip()
    c.draw(body, in: rect)
    return c.makeImage()!
}
func write(_ d: Data, _ p: String) { try! d.write(to: URL(fileURLWithPath: p)) }

let brand = "design/01-brand/app-icons", assets = "apps/desktop/assets/icons"
for n in [16, 32, 64, 128, 256, 512, 1024] { write(png(icon(n, fraction: 1)), "\(brand)/synthesis-app-icon-\(n).png") }
write(png(icon(256, fraction: 1)), "\(assets)/synthesis-256.png")
write(png(icon(512, fraction: 1)), "\(assets)/synthesis-512.png")

// .ico with PNG-compressed entries
let sizes = [16, 24, 32, 48, 64, 128, 256]
let imgs = sizes.map { png(icon($0, fraction: 1)) }
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
    write(png(icon(n, fraction: 824.0 / 1024)), "\(set)/icon_\(n)x\(n).png")
    write(png(icon(n * 2, fraction: 824.0 / 1024)), "\(set)/icon_\(n)x\(n)@2x.png")
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", "\(assets)/synthesis.icns"]
try! p.run(); p.waitUntilExit()
precondition(p.terminationStatus == 0, "iconutil failed")
