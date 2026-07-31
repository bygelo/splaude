import AVFoundation

/// Taps the default input device and hands back 16 kHz mono signed-16 PCM,
/// plus a smoothed RMS level for the menu bar icon.
final class AudioCapture {

    /// Emitted on a background queue.
    var onAudio: ((Data) -> Void)?
    /// Emitted on the main queue, 0…1.
    var onLevel: ((Float) -> Void)?
    var onFailure: ((String) -> Void)?

    private let engine = AVAudioEngine()
    private var converter: AVAudioConverter?
    private var target: AVAudioFormat?
    private var isRunning = false
    private var level: Float = 0

    /// Totals for the run, so the log can distinguish "mic sent nothing" from
    /// "mic sent silence" from "mic was fine, the socket was not".
    private var byteSent = 0
    private var peakLevel: Float = 0
    private(set) var lastPeak: Float = 0

    private static let tapBufferSize: AVAudioFrameCount = 2048

    func requestPermission(_ completion: @escaping (Bool) -> Void) {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            completion(true)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { granted in
                DispatchQueue.main.async { completion(granted) }
            }
        default:
            completion(false)
        }
    }

    func start(format: SpeechAudioFormat) {
        guard !isRunning else { return }

        guard let target = AVAudioFormat(commonFormat: .pcmFormatInt16,
                                         sampleRate: format.sampleRate,
                                         channels: AVAudioChannelCount(format.channelCount),
                                         interleaved: true) else {
            onFailure?("could not build the 16 kHz output format")
            return
        }
        self.target = target

        let input = engine.inputNode
        let source = input.inputFormat(forBus: 0)

        guard source.sampleRate > 0 else {
            onFailure?("no input device available")
            return
        }

        guard let converter = AVAudioConverter(from: source, to: target) else {
            onFailure?("could not convert \(Int(source.sampleRate)) Hz input to 16 kHz")
            return
        }
        self.converter = converter

        input.installTap(onBus: 0, bufferSize: Self.tapBufferSize, format: source) { [weak self] buffer, _ in
            self?.process(buffer, from: source, to: target)
        }

        do {
            engine.prepare()
            try engine.start()
            isRunning = true
            byteSent = 0
            peakLevel = 0
            Diagnostic.log("audio", "capturing \(Int(source.sampleRate)) Hz × \(source.channelCount)ch → \(Int(target.sampleRate)) Hz mono int16")
        } catch {
            input.removeTap(onBus: 0)
            onFailure?("microphone could not start: \(error.localizedDescription)")
        }
    }

    func stop() {
        guard isRunning else { return }
        isRunning = false

        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        converter = nil

        lastPeak = peakLevel
        Diagnostic.log("audio", "sent \(byteSent) bytes, peak level \(String(format: "%.2f", peakLevel))")

        level = 0
        DispatchQueue.main.async { [weak self] in self?.onLevel?(0) }
    }

    // MARK: - Conversion

    private func process(_ buffer: AVAudioPCMBuffer, from source: AVAudioFormat, to target: AVAudioFormat) {
        guard let converter else { return }

        updateLevel(buffer)

        // Output capacity has to cover the resampling ratio, with slack for the
        // converter's internal latency.
        let ratio = target.sampleRate / source.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1024

        guard let output = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: capacity) else { return }

        var supplied = false
        var conversionError: NSError?

        let status = converter.convert(to: output, error: &conversionError) { _, outStatus in
            if supplied {
                outStatus.pointee = .noDataNow
                return nil
            }
            supplied = true
            outStatus.pointee = .haveData
            return buffer
        }

        guard status != .error, output.frameLength > 0 else { return }

        guard let channel = output.int16ChannelData else { return }
        let byteCount = Int(output.frameLength) * MemoryLayout<Int16>.size * Int(target.channelCount)
        let data = Data(bytes: channel[0], count: byteCount)

        byteSent += byteCount
        onAudio?(data)
    }

    private func updateLevel(_ buffer: AVAudioPCMBuffer) {
        guard let channel = buffer.floatChannelData, buffer.frameLength > 0 else { return }

        var sum: Float = 0
        let samples = channel[0]
        for index in 0..<Int(buffer.frameLength) {
            let sample = samples[index]
            sum += sample * sample
        }

        let rms = sqrt(sum / Float(buffer.frameLength))
        // Map a useful speech range (-50 dBFS … 0) onto 0…1.
        let decibel = 20 * log10(max(rms, 1e-7))
        let normalised = max(0, min(1, (decibel + 50) / 50))

        // Fast attack, slow release, so the icon reads as speech rather than noise.
        level = normalised > level ? normalised : level * 0.8 + normalised * 0.2
        peakLevel = max(peakLevel, normalised)

        let snapshot = level
        DispatchQueue.main.async { [weak self] in self?.onLevel?(snapshot) }
    }
}
