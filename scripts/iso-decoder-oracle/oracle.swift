// Independent ISO 21496-1 / Ultra HDR v1 decoder oracle for nc's gain-map JPEGs.
//
// Apple ImageIO is an independent implementation of both dialects, so what it
// reports about nc's bytes is evidence nc's serializers are conformant that
// nc's own reader could never supply.
//
// Build: swiftc -O oracle.swift -o oracle
// Run:   ./oracle <file.jpg> [more.jpg ...]

import Foundation
import ImageIO
import CoreImage
import CoreGraphics

func fmt(_ v: Any?) -> String {
    guard let v else { return "nil" }
    if let n = v as? NSNumber { return "\(n)" }
    if let d = v as? [String: Any] {
        return "{" + d.keys.sorted().map { "\($0): \(fmt(d[$0]))" }.joined(separator: ", ") + "}"
    }
    if let a = v as? [Any] { return "[" + a.map { fmt($0) }.joined(separator: ", ") + "]" }
    if let d = v as? Data { return "<\(d.count) bytes>" }
    return "\(v)"
}

func auxReport(_ src: CGImageSource, _ label: String, _ type: CFString) {
    guard let info = CGImageSourceCopyAuxiliaryDataInfoAtIndex(src, 0, type)
            as? [String: Any] else {
        print("  \(label): ABSENT")
        return
    }
    print("  \(label): PRESENT")
    if let data = info[kCGImageAuxiliaryDataInfoData as String] as? Data {
        print("      data: \(data.count) bytes")
    }
    if let desc = info[kCGImageAuxiliaryDataInfoDataDescription as String] as? [String: Any] {
        print("      description: \(fmt(desc))")
    }
    // Degrade rather than trap: this is a diagnostic tool, and a file whose
    // metadata comes back in an unexpected shape is a finding, not a crash.
    if let meta = info[kCGImageAuxiliaryDataInfoMetadata as String] {
        // `meta as? CGImageMetadata` does not compile — Swift rejects a
        // conditional downcast to a CF type as always-succeeding and points at
        // the CFTypeID comparison instead, so the check is explicit and the
        // force-cast below is guarded by it.
        guard CFGetTypeID(meta as CFTypeRef) == CGImageMetadataGetTypeID() else {
            print("      meta: UNREADABLE (not a CGImageMetadata)")
            return
        }
        let m = meta as! CGImageMetadata
        var lines: [String] = []
        CGImageMetadataEnumerateTagsUsingBlock(m, nil, nil) { path, tag in
            let value = CGImageMetadataTagCopyValue(tag)
            lines.append("\(path) = \(fmt(value))")
            return true
        }
        for line in lines.sorted() { print("      meta: \(line)") }
    }
}

for path in CommandLine.arguments.dropFirst() {
    let url = URL(fileURLWithPath: path)
    print("\n=== \(url.lastPathComponent) ===")
    guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else {
        print("  FAILED to open"); continue
    }
    print("  images in container: \(CGImageSourceGetCount(src))")

    // Which gain-map dialects does ImageIO recognize?
    auxReport(src, "ISO 21496-1 gain map (kCGImageAuxiliaryDataTypeISOGainMap)",
              kCGImageAuxiliaryDataTypeISOGainMap)
    auxReport(src, "Apple/legacy HDR gain map (kCGImageAuxiliaryDataTypeHDRGainMap)",
              kCGImageAuxiliaryDataTypeHDRGainMap)

    // Base image properties.
    if let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [String: Any] {
        let interesting = ["PixelWidth", "PixelHeight", "ProfileName", "Depth",
                           "HasAlpha", "ColorModel"]
        for key in interesting where props[key] != nil {
            print("  base.\(key) = \(fmt(props[key]))")
        }
        if let hdr = props["HDRHeadroom"] { print("  base.HDRHeadroom = \(fmt(hdr))") }
    }

    // SDR decode (an ordinary reader's view).
    if let sdr = CGImageSourceCreateImageAtIndex(src, 0, nil) {
        print("  SDR decode: \(sdr.width)x\(sdr.height), headroom \(sdr.contentHeadroom)")
    } else {
        print("  SDR decode: FAILED")
    }

    // HDR decode — this is the reconstruction step. A decoder that applied the
    // gain map reports a headroom above 1.0.
    let hdrOpts: [CFString: Any] = [kCGImageSourceDecodeRequest: kCGImageSourceDecodeToHDR]
    if let hdr = CGImageSourceCreateImageAtIndex(src, 0, hdrOpts as CFDictionary) {
        print("  HDR decode: \(hdr.width)x\(hdr.height), headroom \(hdr.contentHeadroom)")
    } else {
        print("  HDR decode: FAILED")
    }

    // CoreImage's independent path to the same reconstruction.
    if let ci = CIImage(contentsOf: url, options: [.expandToHDR: true]) {
        print("  CIImage expandToHDR: headroom \(ci.contentHeadroom)")
    } else {
        print("  CIImage expandToHDR: FAILED")
    }
}
