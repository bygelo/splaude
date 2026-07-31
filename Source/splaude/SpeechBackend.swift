import Foundation

/// A streaming speech-to-text session. Everything above this line is backend
/// agnostic, so swapping in a direct Deepgram key or a local model later means
/// adding one file, not touching the app.
protocol SpeechBackend: AnyObject {
    /// Audio contract the capture stage must satisfy.
    static var audioFormat: SpeechAudioFormat { get }

    var delegate: SpeechBackendDelegate? { get set }

    func start()
    /// Raw PCM matching `audioFormat`.
    func send(audio: Data)
    func finish()
}

struct SpeechAudioFormat {
    let sampleRate: Double
    let channelCount: UInt32
    /// Signed 16-bit little-endian PCM. The only encoding any backend here wants.
    static let linear16_16k = SpeechAudioFormat(sampleRate: 16000, channelCount: 1)
}

protocol SpeechBackendDelegate: AnyObject {
    func speechDidOpen()
    /// `isFinal` marks an utterance boundary — the text is committed at that point.
    func speechDidTranscribe(_ text: String, isFinal: Bool)
    func speechDidFail(_ message: String, fatal: Bool)
    func speechDidClose()
}

/// Mirrors the extension's committed/interim bookkeeping: finals accumulate
/// space-joined, and the live display is committed + the pending interim.
struct TranscriptBuffer {
    private(set) var committed = ""

    /// Returns the full text to display after applying this chunk.
    @discardableResult
    mutating func apply(_ text: String, isFinal: Bool) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)

        if isFinal {
            guard !trimmed.isEmpty else { return committed }
            committed = committed.isEmpty ? trimmed : "\(committed) \(trimmed)"
            return committed
        }

        guard !trimmed.isEmpty else { return committed }
        return committed.isEmpty ? trimmed : "\(committed) \(trimmed)"
    }

    mutating func reset() { committed = "" }
}
