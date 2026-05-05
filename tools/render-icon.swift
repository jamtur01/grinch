// Render an emoji to a square PNG using AppKit. Used by the Makefile to
// build Grinch.app's icon from a single emoji string at build time.
//
// usage: swift tools/render-icon.swift <emoji> <size> <output.png>

import AppKit

let args = CommandLine.arguments
guard args.count == 4, let size = Int(args[2]) else {
    FileHandle.standardError.write(
        "usage: render-icon.swift <emoji> <size> <output.png>\n".data(using: .utf8)!)
    exit(1)
}
let emoji = args[1]
let outputPath = args[3]

let imageSize = NSSize(width: size, height: size)
let image = NSImage(size: imageSize)
image.lockFocus()

// 0.85 of the canvas leaves a small breathing margin without wasting space.
let font = NSFont.systemFont(ofSize: CGFloat(size) * 0.85)
let attrs: [NSAttributedString.Key: Any] = [.font: font]
let str = NSAttributedString(string: emoji, attributes: attrs)
let stringSize = str.size()
let origin = NSPoint(
    x: (imageSize.width - stringSize.width) / 2,
    y: (imageSize.height - stringSize.height) / 2
)
str.draw(at: origin)

image.unlockFocus()

guard let tiff = image.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:])
else {
    FileHandle.standardError.write("render-icon: failed to encode PNG\n".data(using: .utf8)!)
    exit(1)
}

let url = URL(fileURLWithPath: outputPath)
do {
    try png.write(to: url)
} catch {
    FileHandle.standardError.write(
        "render-icon: write failed: \(error)\n".data(using: .utf8)!)
    exit(1)
}
