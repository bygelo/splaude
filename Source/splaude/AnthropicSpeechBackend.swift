import Foundation

/// Streams microphone audio to the WebSocket the Claude Code IDE extension uses
/// for its dictation button. The endpoint, query parameter and framing below are
/// taken verbatim from the shipped extension bundle:
///
///   wss://api.anthropic.com/api/ws/speech_to_text/voice_stream
///     ?encoding=linear16&sample_rate=16000&channels=1
///     &endpointing_ms=300&utterance_end_ms=1000&language=en
///     &use_conversation_engine=true&stt_provider=deepgram-nova3
///
/// It is an undocumented internal endpoint authenticated with the Claude Code
/// OAuth token. It can change or disappear without notice.
final class AnthropicSpeechBackend: NSObject, SpeechBackend {

    static let audioFormat = SpeechAudioFormat.linear16_16k

    weak var delegate: SpeechBackendDelegate?

    private let credential: TokenStore.Credential
    private let keyterm: [String]
    private let language: String
    /// Asks the server to shape interim results for typing — punctuated and
    /// cased as they stream, rather than raw words to be cleaned up at the end.
    private let typedInterim: Bool

    private var session: URLSession?
    private var socket: URLSessionWebSocketTask?
    private var keepAlive: DispatchSourceTimer?

    /// Text seen but not yet committed by an endpoint event.
    private var pending = ""
    private var isClosed = false
    /// Set once CloseStream is sent. The server then drops the connection, and
    /// the still-running receive loop sees that as an error — an expected one,
    /// which must not be reported or it buries the real status message.
    private var isFinishing = false

    private let queue = DispatchQueue(label: "com.bygelo.splaude.speech")

    init(credential: TokenStore.Credential,
         keyterm: [String] = Setting.keyterm,
         language: String = Setting.language,
         typedInterim: Bool = Setting.liveTyping) {
        self.credential = credential
        self.keyterm = keyterm
        self.language = language
        self.typedInterim = typedInterim
    }

    // MARK: - Wire constants (from the extension bundle)

    private static let endpoint = "wss://api.anthropic.com/api/ws/speech_to_text/voice_stream"
    private static let keepAliveInterval: TimeInterval = 8
    private static let closeGrace: TimeInterval = 3
    private static let keytermByteBudget = 1024

    private static let keepAliveFrame = #"{"type":"KeepAlive"}"#
    private static let closeFrame = #"{"type":"CloseStream"}"#

    // MARK: - SpeechBackend

    func start() {
        var component = URLComponents(string: Self.endpoint)!
        component.queryItems = [
            URLQueryItem(name: "encoding", value: "linear16"),
            URLQueryItem(name: "sample_rate", value: String(Int(Self.audioFormat.sampleRate))),
            URLQueryItem(name: "channels", value: String(Self.audioFormat.channelCount)),
            URLQueryItem(name: "endpointing_ms", value: "300"),
            URLQueryItem(name: "utterance_end_ms", value: "1000"),
            URLQueryItem(name: "language", value: language),
            URLQueryItem(name: "use_conversation_engine", value: "true"),
            URLQueryItem(name: "stt_provider", value: "deepgram-nova3"),
        ]

        if typedInterim {
            component.queryItems?.append(URLQueryItem(name: "forward_interims", value: "typed"))
        }

        var request = URLRequest(url: component.url!)
        request.setValue("Bearer \(credential.accessToken)", forHTTPHeaderField: "Authorization")
        request.setValue("vscode", forHTTPHeaderField: "x-app")

        let packed = Self.packKeyterm(keyterm)
        if !packed.isEmpty {
            request.setValue(packed, forHTTPHeaderField: "x-config-keyterms")
        }

        let session = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
        self.session = session

        let socket = session.webSocketTask(with: request)
        self.socket = socket
        socket.resume()

        receive()
    }

    func send(audio: Data) {
        queue.async { [weak self] in
            guard let self, !self.isClosed, let socket = self.socket else { return }
            socket.send(.data(audio)) { error in
                guard let error else { return }
                self.fail("audio send failed: \(error.localizedDescription)", fatal: false)
            }
        }
    }

    func finish() {
        queue.async { [weak self] in
            guard let self, !self.isClosed else { return }

            self.isFinishing = true
            Diagnostic.log("socket", "closing")
            self.socket?.send(.string(Self.closeFrame)) { _ in }

            // Give the server a moment to flush a trailing endpoint event.
            self.queue.asyncAfter(deadline: .now() + Self.closeGrace) { [weak self] in
                self?.teardown()
            }
        }
    }

    // MARK: - Receive loop

    private func receive() {
        socket?.receive { [weak self] result in
            guard let self else { return }

            switch result {
            case .failure(let error):
                guard !self.isClosed else { return }
                if self.isFinishing {
                    Diagnostic.log("socket", "closed after CloseStream (expected)")
                } else {
                    self.fail("WebSocket error: \(error.localizedDescription)", fatal: false)
                }
                self.teardown()

            case .success(let message):
                switch message {
                case .string(let text):
                    self.handle(Data(text.utf8))
                case .data(let data):
                    self.handle(data)
                @unknown default:
                    break
                }
                self.receive()
            }
        }
    }

    private func handle(_ data: Data) {
        guard let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = frame["type"] as? String else { return }

        switch type {
        // Both interim and text frames are provisional; the endpoint frame is
        // what actually commits an utterance. This matches the extension.
        case "TranscriptInterim", "TranscriptText":
            guard let text = frame["data"] as? String, !text.isEmpty else { break }
            pending = text
            delegate?.speechDidTranscribe(text, isFinal: false)

        case "TranscriptEndpoint":
            Diagnostic.log("stt", "endpoint — commit \"\(pending)\"")
            flushPending()

        case "TranscriptError":
            fail(frame["description"] as? String ?? "transcription error", fatal: false)

        case "error":
            fail(frame["message"] as? String ?? "server error", fatal: false)

        default:
            break
        }
    }

    private func flushPending() {
        guard !pending.isEmpty else { return }
        let text = pending
        pending = ""
        delegate?.speechDidTranscribe(text, isFinal: true)
    }

    // MARK: - Lifecycle

    private func startKeepAlive() {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.keepAliveInterval, repeating: Self.keepAliveInterval)
        timer.setEventHandler { [weak self] in
            guard let self, !self.isClosed else { return }
            self.socket?.send(.string(Self.keepAliveFrame)) { _ in }
        }
        timer.resume()
        keepAlive = timer
    }

    private func teardown() {
        guard !isClosed else { return }
        isClosed = true

        keepAlive?.cancel()
        keepAlive = nil

        flushPending()

        socket?.cancel(with: .normalClosure, reason: nil)
        socket = nil

        session?.finishTasksAndInvalidate()
        session = nil

        delegate?.speechDidClose()
    }

    private func fail(_ message: String, fatal: Bool) {
        delegate?.speechDidFail(message, fatal: fatal)
    }

    /// Comma-joined, deduped, ASCII-only, truncated to the server's budget —
    /// the same normalisation the extension applies before sending keyterms.
    static func packKeyterm(_ term: [String]) -> String {
        var seen = Set<String>()
        var kept: [String] = []
        var length = 0

        for raw in term {
            let clean = raw
                .replacingOccurrences(of: ",", with: " ")
                .replacingOccurrences(of: "[^\\x20-\\x7E]", with: "", options: .regularExpression)
                .replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression)
                .trimmingCharacters(in: .whitespaces)

            guard !clean.isEmpty, !seen.contains(clean) else { continue }

            let cost = clean.count + (kept.isEmpty ? 0 : 1)
            guard length + cost <= keytermByteBudget else { break }

            seen.insert(clean)
            kept.append(clean)
            length += cost
        }

        return kept.joined(separator: ",")
    }
}

// MARK: - URLSessionWebSocketDelegate

extension AnthropicSpeechBackend: URLSessionWebSocketDelegate {

    func urlSession(_ session: URLSession,
                    webSocketTask: URLSessionWebSocketTask,
                    didOpenWithProtocol protocol: String?) {
        queue.async { [weak self] in
            guard let self else { return }
            self.socket?.send(.string(Self.keepAliveFrame)) { _ in }
            self.startKeepAlive()
        }
        // The 101 response carries whatever metering headers the endpoint uses;
        // this is where the "does it spend quota" question gets answered.
        QuotaWatch.markConnected()
        if let response = webSocketTask.response as? HTTPURLResponse {
            QuotaWatch.record(response)
        }

        Diagnostic.log("socket", "open")
        delegate?.speechDidOpen()
    }

    func urlSession(_ session: URLSession,
                    webSocketTask: URLSessionWebSocketTask,
                    didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
                    reason: Data?) {
        queue.async { [weak self] in self?.teardown() }
    }

    func urlSession(_ session: URLSession,
                    task: URLSessionTask,
                    didCompleteWithError error: Error?) {
        // A 4xx on the upgrade means the credential was rejected — that is fatal
        // and worth surfacing differently from a dropped connection.
        if let response = task.response as? HTTPURLResponse, response.statusCode >= 400 {
            let fatal = (400..<500).contains(response.statusCode)
            let hint = fatal
                ? "credential rejected (HTTP \(response.statusCode)) — run `claude` to re-authenticate"
                : "server error (HTTP \(response.statusCode))"
            fail(hint, fatal: fatal)
        } else if let error, !isClosed, !isFinishing {
            fail("connection failed: \(error.localizedDescription)", fatal: false)
        }

        queue.async { [weak self] in self?.teardown() }
    }
}
