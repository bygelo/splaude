import Foundation

/// `splaude --bench "phrase" ...` — does the bias actually help?
///
/// The honest way to answer that is not two dictations, because you never say a
/// phrase the same way twice and the difference you measure is your own mouth.
/// So this records the microphone **once** into a buffer and replays the very
/// same PCM down two sockets: one carrying the harvested keyterm list, one
/// carrying nothing at all. Every variable except the bias is held fixed.
///
/// Audio is fed back at wall-clock speed rather than as fast as the socket will
/// take it. The endpointing the server does is time-sensitive, and a buffer
/// blasted in one go endpoints differently from speech, which would make the
/// two passes incomparable to a real take.
enum Benchmark {

    /// One phrase, transcribed twice.
    private struct Round {
        let target: String
        let biased: String
        let plain: String
    }

    /// Seconds to record when there is no terminal to press Return on.
    ///
    /// A pipe answers `readLine()` instantly with nil, so a Return-to-stop
    /// prompt run under anything that is not a real terminal records exactly
    /// nothing and reports it as a microphone failure. Detecting that and
    /// timing the take instead is the difference between a benchmark that runs
    /// anywhere and one that only runs when you launch it by hand.
    private static let untimedHold: TimeInterval = 5

    static func run(_ argument: [String]) -> Never {
        var hold = untimedHold
        var synthetic = false
        var cap = Int.max
        var target: [String] = []
        var index = argument.startIndex

        while index < argument.endIndex {
            if argument[index] == "--hold", index + 1 < argument.endIndex,
               let second = TimeInterval(argument[index + 1]) {
                hold = second
                index += 2
            } else if argument[index] == "--say" {
                synthetic = true
                index += 1
            } else if argument[index] == "--terms", index + 1 < argument.endIndex,
                      let count = Int(argument[index + 1]) {
                cap = count
                index += 2
            } else {
                target.append(argument[index])
                index += 1
            }
        }

        guard !target.isEmpty else {
            print("usage: splaude --bench [--hold SECONDS] \"a phrase to say\" [\"another\"] …")
            exit(2)
        }

        let interactive = isatty(FileHandle.standardInput.fileDescriptor) == 1

        let credential: TokenStore.Credential
        do {
            credential = try TokenStore.load()
        } catch {
            print("no Claude Code credential: \(error.localizedDescription)")
            print("run `claude` once in a terminal, then retry.")
            exit(1)
        }

        let keyterm = Array(Setting.wireKeyterm.prefix(cap))
        let packed = AnthropicSpeechBackend.packKeyterm(keyterm)

        print("splaude benchmark")
        print("  project   \(Project.active()?.name ?? "none")")
        print("  bias      \(packed.count) of 1024 characters, \(keyterm.count) terms")
        print("""

            Each phrase is recorded once and sent twice — with the bias and \
            without. Same audio both times, so the only difference is the \
            keyterm list.
            """)

        var round: [Round] = []

        if synthetic {
            print("""

                  Speaking each phrase with `say` instead of recording it.                   Synthetic speech is not your voice and a recogniser does not                   treat it as such, so the absolute scores mean little — but                   both passes get byte-identical audio, so the difference                   between them is still a clean measurement of the bias.
                """)
        } else if !interactive {
            print("""
                  Not a terminal, so there is no Return to press: each phrase                   records for \(Int(hold)) seconds on a countdown instead.                   Pass --hold SECONDS to change that.
                """)
        }

        for (index, phrase) in target.enumerated() {
            print("\n\u{001B}[1m[\(index + 1)/\(target.count)] Say:  \(phrase)\u{001B}[0m")

            if synthetic {
                // no capture at all — the phrase is spoken into a file below
            } else if interactive {
                print("  Press Return to start recording…", terminator: "")
                _ = readLine()
            } else {
                for count in stride(from: 3, through: 1, by: -1) {
                    print("  \(count)…", terminator: "")
                    fflush(stdout)
                    Thread.sleep(forTimeInterval: 1)
                }
            }

            let captured = synthetic
                ? speak(phrase)
                : record(hold: interactive ? nil : hold)

            guard let audio = captured else {
                print("  no audio captured — skipping")
                continue
            }

            let second = Double(audio.count) / 32_000
            print(String(format: "  captured %.1fs (%d KB) — replaying twice", second, audio.count / 1024))

            guard let biased = transcribe(audio, credential: credential, keyterm: keyterm) else {
                print("  with bias:    (failed)")
                continue
            }
            print("  with bias:    \(biased.isEmpty ? "(nothing)" : biased)")

            guard let plain = transcribe(audio, credential: credential, keyterm: []) else {
                print("  without bias: (failed)")
                continue
            }
            print("  without bias: \(plain.isEmpty ? "(nothing)" : plain)")

            round.append(Round(target: phrase, biased: biased, plain: plain))
        }

        report(round)
        exit(0)
    }

    // MARK: - Capture

    /// Records until Return, or for `hold` seconds when there is no terminal,
    /// returning the whole take as one PCM buffer.
    private static func record(hold: TimeInterval?) -> Data? {
        let capture = AudioCapture()
        var buffer = Data()
        let lock = NSLock()

        let permitted = DispatchSemaphore(value: 0)
        var granted = false
        capture.requestPermission { allowed in
            granted = allowed
            permitted.signal()
        }
        permitted.wait()
        guard granted else {
            print("  microphone permission refused")
            return nil
        }

        capture.onAudio = { chunk in
            lock.lock()
            buffer.append(chunk)
            lock.unlock()
        }
        capture.onFailure = { message in print("  audio: \(message)") }

        capture.start(format: AnthropicSpeechBackend.audioFormat)

        if let hold {
            print("  \u{001B}[31m● recording \(Int(hold))s\u{001B}[0m — say it now")
            fflush(stdout)
            Thread.sleep(forTimeInterval: hold)
        } else {
            print("  \u{001B}[31m● recording\u{001B}[0m — press Return when done…", terminator: "")
            _ = readLine()
        }

        capture.stop()

        // The tap runs on its own queue; give the last buffers a moment to land
        // rather than truncating the tail of the phrase being measured.
        Thread.sleep(forTimeInterval: 0.2)

        lock.lock()
        defer { lock.unlock() }
        return buffer.isEmpty ? nil : buffer
    }

    /// Renders a phrase with `say` straight into the wire format.
    ///
    /// `LEI16@16000` is signed 16-bit little-endian at 16 kHz, which is exactly
    /// what the endpoint wants, so the header is the only thing to strip. A
    /// canonical WAV from `say` has a 44-byte header; the chunk is located
    /// rather than assumed, because a longer one would shift the whole stream
    /// and turn speech into noise.
    private static func speak(_ phrase: String) -> Data? {
        let path = FileManager.default.temporaryDirectory
            .appendingPathComponent("splaude-bench-\(UUID().uuidString).wav")
        defer { try? FileManager.default.removeItem(at: path) }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/say")
        process.arguments = [
            "-o", path.path, "--data-format=LEI16@16000", "--channels=1", phrase,
        ]

        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            print("  say failed: \(error.localizedDescription)")
            return nil
        }
        guard process.terminationStatus == 0, let file = try? Data(contentsOf: path) else {
            print("  say produced nothing")
            return nil
        }

        return pcmBody(file)
    }

    /// The `data` chunk of a RIFF/WAVE file, or nil if this is not one.
    static func pcmBody(_ file: Data) -> Data? {
        guard file.count > 12,
              file[0..<4].elementsEqual("RIFF".utf8),
              file[8..<12].elementsEqual("WAVE".utf8)
        else { return nil }

        var offset = 12
        while offset + 8 <= file.count {
            let identifier = file[offset..<offset + 4]
            let size = file[(offset + 4)..<(offset + 8)]
                .reversed()
                .reduce(into: UInt32(0)) { $0 = $0 << 8 | UInt32($1) }
            let body = offset + 8

            if identifier.elementsEqual("data".utf8) {
                let end = min(body + Int(size), file.count)
                return body < end ? file.subdata(in: body..<end) : nil
            }
            // Chunks are word-aligned, so an odd size carries a pad byte.
            offset = body + Int(size) + (size % 2 == 1 ? 1 : 0)
        }

        return nil
    }

    // MARK: - Replay

    /// Streams one buffer through a fresh socket and returns the committed text.
    private static func transcribe(
        _ audio: Data, credential: TokenStore.Credential, keyterm: [String]
    ) -> String? {
        let sink = Sink()
        let backend = AnthropicSpeechBackend(
            credential: credential,
            keyterm: keyterm,
            language: Setting.language,
            // Interim shaping changes the text; a benchmark wants the committed
            // transcript, which is what a finished take actually inserts.
            typedInterim: false
        )
        backend.delegate = sink
        backend.start()

        guard sink.opened.wait(timeout: .now() + 15) == .success else { return nil }

        // 100 ms of 16 kHz mono int16 is 3200 bytes. Paced to wall clock so the
        // server endpoints the way it would on live speech.
        let chunk = 3_200
        var offset = 0
        while offset < audio.count {
            let end = min(offset + chunk, audio.count)
            backend.send(audio: audio.subdata(in: offset..<end))
            offset = end
            Thread.sleep(forTimeInterval: 0.1)
        }

        backend.finish()
        _ = sink.closed.wait(timeout: .now() + 15)
        return sink.text
    }

    /// Accumulates finals the way a real take does.
    private final class Sink: SpeechBackendDelegate {
        let opened = DispatchSemaphore(value: 0)
        let closed = DispatchSemaphore(value: 0)

        private var buffer = TranscriptBuffer()
        private let lock = NSLock()

        var text: String {
            lock.lock()
            defer { lock.unlock() }
            return buffer.committed
        }

        func speechDidOpen() { opened.signal() }

        func speechDidTranscribe(_ text: String, isFinal: Bool) {
            guard isFinal else { return }
            lock.lock()
            buffer.apply(text, isFinal: true)
            lock.unlock()
        }

        func speechDidFail(_ message: String, fatal: Bool) {
            print("  \(fatal ? "fatal" : "warning"): \(message)")
            if fatal { opened.signal() }
        }

        func speechDidClose() {
            opened.signal()
            closed.signal()
        }
    }

    // MARK: - Scoring

    private static func report(_ round: [Round]) {
        guard !round.isEmpty else {
            print("\nnothing recorded.")
            return
        }

        print("\n\u{001B}[1mresult\u{001B}[0m")

        var biasedHit = 0
        var plainHit = 0
        var total = 0

        for one in round {
            // Scored per word of the target, not on the whole sentence. A
            // phrase can be right in every way that matters and still differ by
            // a comma, and a pass/fail on the whole string would call that a
            // miss and hide the word actually being measured.
            let want = normalise(one.target)
            let withBias = Set(normalise(one.biased))
            let without = Set(normalise(one.plain))

            var line: [String] = []
            for word in want {
                total += 1
                let a = withBias.contains(word)
                let b = without.contains(word)
                if a { biasedHit += 1 }
                if b { plainHit += 1 }

                switch (a, b) {
                case (true, true): line.append("  \(word)")
                case (true, false): line.append("\u{001B}[32m+ \(word)\u{001B}[0m")
                case (false, true): line.append("\u{001B}[31m- \(word)\u{001B}[0m")
                case (false, false): line.append("\u{001B}[33m✗ \(word)\u{001B}[0m")
                }
            }

            print("\n  \(one.target)")
            print("    " + line.joined(separator: "  "))
        }

        print("""

              \(biasedHit)/\(total) target words with the bias
              \(plainHit)/\(total) without

            \u{001B}[32m+\u{001B}[0m only the biased pass got it   \
            \u{001B}[31m-\u{001B}[0m only the plain pass got it   \
            \u{001B}[33m✗\u{001B}[0m neither
            """)

        if biasedHit == plainHit {
            print("""

                  No difference on this sample. Either the bias is not reaching
                  the server, or these words were never the problem.
                """)
        }
    }

    /// Lowercased words, punctuation stripped. Comparing raw strings would
    /// score a trailing full stop as a transcription error.
    private static func normalise(_ text: String) -> [String] {
        text.lowercased()
            .components(separatedBy: CharacterSet.alphanumerics.inverted)
            .filter { !$0.isEmpty }
    }
}
