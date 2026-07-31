import Foundation

/// Append-only log at ~/Library/Logs/splaude.log, plus stderr when run from a
/// terminal. A menu bar app has nowhere to print, and every failure mode here
/// (permission, socket, empty audio, refused paste) looks identical from the
/// outside — "nothing happened".
enum Diagnostic {

    static let path = FileManager.default
        .homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/splaude.log")

    private static let queue = DispatchQueue(label: "com.bygelo.splaude.log")
    private static let stamp: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss.SSS"
        return formatter
    }()

    /// Kept so the menu can show the last few lines without opening Console.
    private(set) static var recent: [String] = []

    static func log(_ area: String, _ message: String) {
        let line = "\(stamp.string(from: Date())) [\(area)] \(message)"

        queue.async {
            recent.append(line)
            if recent.count > 40 { recent.removeFirst() }

            FileHandle.standardError.write(Data((line + "\n").utf8))

            guard let data = (line + "\n").data(using: .utf8) else { return }
            if let handle = try? FileHandle(forWritingTo: path) {
                defer { try? handle.close() }
                _ = try? handle.seekToEnd()
                try? handle.write(contentsOf: data)
            } else {
                try? data.write(to: path)
            }
        }
    }

    /// Marks a run boundary so an old log is not mistaken for the current one.
    static func session(_ note: String) {
        log("session", "──── \(note) ────")
    }
}
