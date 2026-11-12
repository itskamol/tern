// Prints the size of each window of a process, "WIDTHxHEIGHT" per line, in points.
// Used by scripts/measure-memory.sh to confirm the window it measured is the size the ticket
// asks for. Run: measure-window PID
import CoreGraphics
import Foundation

guard CommandLine.arguments.count == 2, let pid = Int32(CommandLine.arguments[1]) else {
    FileHandle.standardError.write(Data("usage: measure-window PID\n".utf8))
    exit(2)
}
let windows = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows where (window[kCGWindowOwnerPID as String] as? Int32) == pid {
    guard let bounds = window[kCGWindowBounds as String] as? [String: Any],
        let width = bounds["Width"] as? Double, let height = bounds["Height"] as? Double
    else { continue }
    print("\(Int(width))x\(Int(height))")
}
