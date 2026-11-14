// Where the windows are: what is on the current Space, and every window of the named app with
// whether it is in view. For when a dev-check window "is not there": a full-screen app keeps it
// on another Space (see the header of scripts/dev-check.sh).
//
//   swift scripts/onscreen-windows.swift [app name, default TernDev]
import CoreGraphics

let app = CommandLine.arguments.dropFirst().first ?? "TernDev"
let all = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []

func describe(_ w: [String: Any]) -> String {
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    let owner = w[kCGWindowOwnerName as String] as? String ?? "?"
    let x = b["X"] ?? "?", y = b["Y"] ?? "?", width = b["Width"] ?? "?", height = b["Height"] ?? "?"
    return "\(owner) at \(x),\(y) size \(width)x\(height)"
}

func isNormal(_ w: [String: Any]) -> Bool {
    let layer = w[kCGWindowLayer as String] as? Int ?? -1
    let height = (w[kCGWindowBounds as String] as? [String: Any])?["Height"] as? Double ?? 0
    return layer == 0 && height >= 100
}

print("On the current Space:")
for w in all where isNormal(w) && (w[kCGWindowIsOnscreen as String] as? Bool ?? false) {
    print("  " + describe(w))
}
print("\(app):")
let mine = all.filter { isNormal($0) && ($0[kCGWindowOwnerName as String] as? String) == app }
if mine.isEmpty { print("  no window") }
for w in mine {
    let shown = (w[kCGWindowIsOnscreen as String] as? Bool ?? false) ? "in view" : "NOT in view"
    print("  " + describe(w) + ", " + shown)
}
