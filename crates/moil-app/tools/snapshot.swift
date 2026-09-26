// Renders an HTML file to a PNG offscreen with WebKit, the engine the app's
// webviews use:
//
//   swift snapshot.swift page.html out.png [width] [light|dark] [top] [height]
//
// `width` is in CSS pixels (default 900). The appearance defaults to the
// system's. `top` and `height` clip the snapshot to part of the page, in CSS
// pixels; `top` may also be a CSS selector such as `#packages`, to start at
// that element. By default the snapshot covers the whole page, up to 6000
// pixels.
import AppKit
import WebKit

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write(
        "usage: snapshot page.html out.png [width] [light|dark] [top] [height]\n".data(using: .utf8)!)
    exit(2)
}
let input = URL(fileURLWithPath: args[1])
let output = URL(fileURLWithPath: args[2])
let width = args.count > 3 ? Double(args[3]) ?? 900 : 900
let appearance: NSAppearance? = switch args.count > 4 ? args[4] : "" {
case "dark": NSAppearance(named: .darkAqua)
case "light": NSAppearance(named: .aqua)
default: nil
}
let topArg = args.count > 5 ? args[5] : "0"
let clipHeight = args.count > 6 ? Double(args[6]) : nil

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)
if let appearance { app.appearance = appearance }

final class Delegate: NSObject, WKNavigationDelegate {
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        let selector = topArg.replacingOccurrences(of: "'", with: "\\'")
        let measure = Double(topArg) != nil
            ? "[document.documentElement.scrollHeight, \(topArg)]"
            : "[document.documentElement.scrollHeight, document.querySelector('\(selector)').getBoundingClientRect().top + window.scrollY]"
        webView.evaluateJavaScript(measure) { result, _ in
            let values = (result as? [Double]) ?? [800, 0]
            let pageHeight = values[0]
            let top = values[1]
            let frameHeight = clipHeight.map { min(top + $0, pageHeight) } ?? min(pageHeight, 6000)
            webView.frame = NSRect(x: 0, y: 0, width: width, height: frameHeight)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                let config = WKSnapshotConfiguration()
                if let clipHeight {
                    config.rect = NSRect(x: 0, y: top, width: width, height: min(clipHeight, frameHeight - top))
                }
                webView.takeSnapshot(with: config) { image, error in
                    guard let image, let tiff = image.tiffRepresentation,
                          let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) else {
                        FileHandle.standardError.write("snapshot failed: \(String(describing: error))\n".data(using: .utf8)!)
                        exit(1)
                    }
                    try! png.write(to: output)
                    exit(0)
                }
            }
        }
    }
}

let webView = WKWebView(frame: NSRect(x: 0, y: 0, width: width, height: 800))
if let appearance { webView.appearance = appearance }
let delegate = Delegate()
webView.navigationDelegate = delegate
webView.loadFileURL(input, allowingReadAccessTo: input.deletingLastPathComponent())
DispatchQueue.main.asyncAfter(deadline: .now() + 15) { exit(3) }
app.run()
