// Regenerates the app icons (macOS only: CoreGraphics + iconutil). Run from the repo root:
//   swift scripts/make-icons.swift [--variant a|b|c]     (default a; see VARIANTS below)
// Art: the brand symbol (transparent master) lit silver-white on a gradient background, cut to the macOS squircle.
// Writes design/01-brand/app-icons/* (full bleed, Linux/Windows) and apps/desktop/assets/icons/* (.icns uses the Apple grid: 824 body in 1024).
import CoreGraphics
import CoreImage
import Foundation
import ImageIO
import UniformTypeIdentifiers

// ---- Variants: gradient stops (position along the top-left to bottom-right diagonal, sRGB). The symbol sits near 0.5. ----
typealias RGB = (Int, Int, Int)
let VARIANTS: [String: [(CGFloat, RGB)]] = [
    "a": [(0, (11, 42, 74)), (0.50, (15, 76, 117)), (0.85, (13, 115, 119)), (1, (20, 163, 161))],  // ocean blue to teal (default)
    "b": [(0, (6, 53, 43)), (0.70, (12, 100, 76)), (0.92, (15, 122, 92)), (1, (31, 181, 138))],   // emerald to teal green
    "c": [(0, (100, 36, 10)), (0.70, (150, 58, 22)), (0.92, (217, 98, 43)), (1, (242, 165, 65))], // copper/amber to rust
]
// Symbol look: white veil over the symbol (frosted silver), specular on its upper-left, neutral bloom behind it.
let glyphLift: CGFloat = 0.72, specularAlpha: CGFloat = 0.7, bloomAlpha: CGFloat = 0.05
let symbolPath = "design/01-brand/logo/png/synthesis-symbol-transparent-1024.png"

setvbuf(stdout, nil, _IONBF, 0)
let args = CommandLine.arguments
let variant = args.firstIndex(of: "--variant").map { args[$0 + 1] } ?? "a"
guard let stops = VARIANTS[variant] else { fatalError("unknown variant \(variant): use a, b or c") }

let cs = CGColorSpace(name: CGColorSpace.sRGB)!
func rgb(_ c: RGB, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [CGFloat(c.0) / 255, CGFloat(c.1) / 255, CGFloat(c.2) / 255, a])!
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
func pixels(_ i: CGImage) -> [UInt8] {
    let c = ctx(i.width)
    c.draw(i, in: CGRect(x: 0, y: 0, width: i.width, height: i.width))
    return Array(UnsafeBufferPointer(start: c.data!.assumingMemoryBound(to: UInt8.self), count: i.width * 4 * i.width))
}
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

/// The 1024 square art (no mask): gradient ground, light source, rim, inner shadow, then the lit symbol (own layer, own shadow).
/// Returns the art and the ground without the symbol, plus the symbol's alpha, for the checks below.
func makeArt() -> (art: CGImage, ground: CGImage, symbol: CGImage) {
    func ground(_ c: CGContext) {
        let g = CGGradient(colorsSpace: cs, colors: stops.map { rgb($0.1) } as CFArray, locations: stops.map { $0.0 })!
        let opts: CGGradientDrawingOptions = [.drawsBeforeStartLocation, .drawsAfterEndLocation]
        c.drawLinearGradient(g, start: CGPoint(x: 0, y: 1024), end: CGPoint(x: 1024, y: 0), options: opts)  // CG origin is bottom-left
        let rg = { (x: CGFloat, y: CGFloat, r: CGFloat, a: CGFloat) in
            c.drawRadialGradient(CGGradient(colorsSpace: cs, colors: [rgb((255, 255, 255), a), rgb((255, 255, 255), 0)] as CFArray, locations: [0, 1])!,
                                 startCenter: CGPoint(x: x, y: y), startRadius: 0, endCenter: CGPoint(x: x, y: y), endRadius: r, options: [])
        }
        rg(200, 860, 620, 0.16)          // light source, top-left
        rg(512, 512, 430, bloomAlpha)    // soft bloom behind the symbol
        c.drawLinearGradient(CGGradient(colorsSpace: cs, colors: [rgb((0, 0, 0), 0), rgb((0, 0, 0), 0.12)] as CFArray, locations: [0, 1])!,
                             start: CGPoint(x: 0, y: 150), end: CGPoint(x: 0, y: 0), options: [])  // inner shadow, bottom
    }
    let gc = ctx(1024); ground(gc)
    let groundImg = gc.makeImage()!
    // symbol layer: frosted silver (white veil only where the symbol is) plus a specular on its upper-left
    let sg = ctx(1024), s: CGFloat = 1024 * 0.80
    sg.draw(load(symbolPath), in: CGRect(x: 512 - s / 2, y: 512 - s / 2, width: s, height: s))
    sg.setBlendMode(.sourceAtop)
    sg.setFillColor(rgb((255, 255, 255), glyphLift)); sg.fill(CGRect(x: 0, y: 0, width: 1024, height: 1024))
    let spec = CGGradient(colorsSpace: cs, colors: [rgb((255, 255, 255), specularAlpha), rgb((255, 255, 255), 0)] as CFArray, locations: [0, 1])!
    sg.drawLinearGradient(spec, start: CGPoint(x: 512 - s * 0.45, y: 512 + s * 0.45), end: CGPoint(x: 512, y: 512), options: [])
    let symbol = sg.makeImage()!
    let c = ctx(1024)
    c.draw(groundImg, in: CGRect(x: 0, y: 0, width: 1024, height: 1024))
    c.setShadow(offset: CGSize(width: 0, height: -26), blur: 40, color: rgb((0, 0, 0), 0.35))
    c.draw(symbol, in: CGRect(x: 0, y: 0, width: 1024, height: 1024))
    c.setShadow(offset: .zero, blur: 0)
    // thin lighter rim, brighter at the top, inside the body
    let body = CGRect(x: 0, y: 0, width: 1024, height: 1024)
    c.saveGState()
    c.addPath(squircle(body)); c.clip()
    c.addPath(squircle(body)); c.setLineWidth(10)
    c.replacePathWithStrokedPath(); c.clip()
    c.drawLinearGradient(CGGradient(colorsSpace: cs, colors: [rgb((255, 255, 255), 0.35), rgb((255, 255, 255), 0.06)] as CFArray, locations: [0, 1])!,
                         start: CGPoint(x: 0, y: 1024), end: CGPoint(x: 0, y: 0), options: [])
    c.restoreGState()
    return (c.makeImage()!, groundImg, symbol)
}
let (art1024, groundImg, symbolImg) = makeArt()

/// Checks on the art: no pixel darker than max(R,G,B) 40 in the squircle body (the ground and the symbol's shadow), no purple hue
/// (250-330 degrees, saturation > 0.15), and the symbol's median luminance against its surrounding ground at least 4.5:1.
func check() {
    let a = pixels(art1024), g = pixels(groundImg), sy = pixels(symbolImg)
    func lum(_ p: [UInt8], _ k: Int) -> Double {
        func f(_ v: UInt8) -> Double { let x = Double(v) / 255; return x <= 0.04045 ? x / 12.92 : pow((x + 0.055) / 1.055, 2.4) }
        return 0.2126 * f(p[k * 4]) + 0.7152 * f(p[k * 4 + 1]) + 0.0722 * f(p[k * 4 + 2])
    }
    var dark = 0, purple = 0, sym = [Double](), ring = 0.0, ringN = 0.0
    for y in stride(from: 0, to: 1024, by: 2) { for x in stride(from: 0, to: 1024, by: 2) {
        let k = y * 1024 + x
        let inBody = squircle(CGRect(x: 40, y: 40, width: 944, height: 944)).contains(CGPoint(x: x, y: y))  // inset: away from the rim
        if inBody {
            let r = Double(a[k * 4]), gg = Double(a[k * 4 + 1]), b = Double(a[k * 4 + 2]), mx = max(r, gg, b), mn = min(r, gg, b)
            if mx < 40 { dark += 1 }
            if mx > 0, (mx - mn) / mx > 0.15 {
                var h = mx == r ? 60 * ((gg - b) / (mx - mn)) : mx == gg ? 60 * (2 + (b - r) / (mx - mn)) : 60 * (4 + (r - gg) / (mx - mn))
                if h < 0 { h += 360 }
                if h >= 250 && h <= 330 { purple += 1 }
            }
        }
        if sy[k * 4 + 3] > 250 { sym.append(lum(sy, k)) }
        else if sy[k * 4 + 3] == 0, hypot(Double(x) - 512, Double(y) - 512) < 400 { ring += lum(g, k); ringN += 1 }
    } }
    sym.sort()
    let ratio = (sym[sym.count / 2] + 0.05) / (ring / ringN + 0.05)
    print("variant \(variant): dark px \(dark), purple px \(purple), symbol/ground contrast \(String(format: "%.2f", ratio)):1 (symbol median lum \(String(format: "%.2f", sym[sym.count / 2])), ground \(String(format: "%.3f", ring / ringN)))")
    precondition(dark == 0 && purple == 0, "ground too dark or purple")
    precondition(ratio >= 4.5, "symbol contrast below 4.5:1")
}
check()

/// n px canvas with the squircle body at `fraction` of it (1.0 = full bleed, 824/1024 = Apple grid); no shadow.
/// Each size is resampled from the art and lightly sharpened before the mask, so the mask edge gets no halo.
func icon(_ n: Int, fraction: CGFloat) -> CGImage {
    let b = (CGFloat(n) * fraction).rounded()
    let body = Int(b) == 1024 ? art1024 : render(resample(CIImage(cgImage: art1024), to: Int(b), sharpen: 0.8, b <= 256 ? 0.35 : 0.15))
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
