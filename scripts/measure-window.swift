// Window facts for scripts/measure-memory.sh, from the window server.
//
//   measure-window PID           the size of each of the process's windows, "WIDTHxHEIGHT" per
//                                line, in points
//   measure-window --visible PID the percentage of the process's main window (its largest normal
//                                window) that no other app's window covers, 0 when it is not on
//                                screen
//
// The memory readings only mean something while the window is drawn, and macOS stops drawing a
// window that is fully covered or off screen. Being the front app is not needed: a window that
// shows is drawn. A script cannot make an app frontmost here anyway (no Accessibility grant), so
// the script asks "can it be seen?" instead of "is it in front?".
import CoreGraphics
import Foundation

func usage() -> Never {
    FileHandle.standardError.write(Data("usage: measure-window [--visible] PID\n".utf8))
    exit(2)
}

func rect(_ window: [String: Any]) -> CGRect? {
    guard let b = window[kCGWindowBounds as String] as? [String: Any],
        let x = b["X"] as? Double, let y = b["Y"] as? Double,
        let w = b["Width"] as? Double, let h = b["Height"] as? Double
    else { return nil }
    return CGRect(x: x, y: y, width: w, height: h)
}

func owner(_ window: [String: Any]) -> Int32? { window[kCGWindowOwnerPID as String] as? Int32 }
func layer(_ window: [String: Any]) -> Int { window[kCGWindowLayer as String] as? Int ?? -1 }
func alpha(_ window: [String: Any]) -> Double { window[kCGWindowAlpha as String] as? Double ?? 1 }

let args = Array(CommandLine.arguments.dropFirst())
let visible = args.first == "--visible"
guard let last = args.last, let pid = Int32(last), args.count == (visible ? 2 : 1) else { usage() }

if !visible {
    let windows = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
    for window in windows where owner(window) == pid {
        guard let r = rect(window) else { continue }
        print("\(Int(r.width))x\(Int(r.height))")
    }
    exit(0)
}

// On-screen windows come front to back; everything before ours, of another app, at the normal
// window layer and not transparent, may cover it.
let onScreen = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
let mine = onScreen.enumerated()
    .filter { owner($0.element) == pid && layer($0.element) == 0 }
    .compactMap { item in rect(item.element).map { (index: item.offset, rect: $0) } }
    .max { $0.rect.width * $0.rect.height < $1.rect.width * $1.rect.height }
guard let main = mine else {
    print(0)
    exit(0)
}
let above = onScreen.prefix(main.index)
    .filter { owner($0) != pid && layer($0) == 0 && alpha($0) > 0 }
    .compactMap(rect)
// A 40×40 grid over the window: the share of points no window above it covers.
let steps = 40
var seen = 0
for i in 0..<steps {
    for j in 0..<steps {
        let p = CGPoint(
            x: main.rect.minX + (Double(i) + 0.5) * main.rect.width / Double(steps),
            y: main.rect.minY + (Double(j) + 0.5) * main.rect.height / Double(steps))
        if !above.contains(where: { $0.contains(p) }) { seen += 1 }
    }
}
print(seen * 100 / (steps * steps))
