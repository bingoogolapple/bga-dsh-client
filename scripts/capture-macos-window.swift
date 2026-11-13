import CoreGraphics
import Foundation
import ImageIO
import Vision

func largestWindow(for pid: Int) -> CGWindowID? {
    let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    return windows.compactMap { window -> (CGWindowID, CGFloat)? in
        guard window[kCGWindowOwnerPID as String] as? Int == pid,
              window[kCGWindowLayer as String] as? Int == 0,
              let number = window[kCGWindowNumber as String] as? NSNumber,
              let bounds = window[kCGWindowBounds as String] as? NSDictionary,
              let frame = CGRect(dictionaryRepresentation: bounds as CFDictionary),
              frame.width >= 600, frame.height >= 400 else {
            return nil
        }
        return (number.uint32Value, frame.width * frame.height)
    }.max(by: { $0.1 < $1.1 })?.0
}

func containsDshInterface(_ url: URL) throws -> Bool {
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
          image.width >= 600, image.height >= 400 else {
        return false
    }

    // Exclude the client toolbar. These strings belong to dsh's rendered Web UI.
    let request = VNRecognizeTextRequest()
    request.recognitionLanguages = ["zh-Hans", "en-US"]
    request.regionOfInterest = CGRect(x: 0, y: 0, width: 1, height: 0.89)
    try VNImageRequestHandler(cgImage: image).perform([request])
    let labels = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
        .joined(separator: " ").lowercased().filter { $0.isLetter || $0.isNumber }
    return [
        "newsession", "newchat", "新会话", "新建会话", "whatcanihelpyouwith", "addworkspace", "添加工作区",
        "addanapikeytogetstarted", "添加一个apikey开始使用", "configurelater", "稍后配置",
    ].contains(where: labels.contains)
}

guard CommandLine.arguments.count == 3, let pid = Int(CommandLine.arguments[1]), pid > 0 else {
    fatalError("Usage: capture-macos-window <app PID> <output PNG>")
}
let output = URL(fileURLWithPath: CommandLine.arguments[2])
let candidate = output.deletingLastPathComponent()
    .appendingPathComponent(".dsh-capture-\(UUID().uuidString).png")
let files = FileManager.default
var lastError = "No visible client window"

// WebKit may still be loading the embedded UI when the HTTP probe succeeds.
for _ in 0..<20 {
    if let id = largestWindow(for: pid) {
        do {
            let capture = Process()
            capture.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
            capture.arguments = ["-x", "-o", "-a", "-l\(id)", candidate.path]
            try capture.run()
            capture.waitUntilExit()
            if capture.terminationStatus == 0, try containsDshInterface(candidate) {
                try files.moveItem(at: candidate, to: output)
                exit(EXIT_SUCCESS)
            }
            lastError = "Window capture did not show the dsh interface (exit \(capture.terminationStatus))"
        } catch {
            lastError = error.localizedDescription
        }
        try? files.removeItem(at: candidate)
    }
    Thread.sleep(forTimeInterval: 2)
}
FileHandle.standardError.write(Data("\(lastError)\n".utf8))
exit(EXIT_FAILURE)
