//
//  LiveAudioPlayer.swift
//  Plays the live stream's AAC track.
//
//  `GlassesAacDepacketizer` (in `LumaCore`) turns RTP payloads into raw AAC frames; this
//  file turns those frames into sound. AAC-LC decodes from a plain
//  `AudioStreamBasicDescription` with no magic cookie, so `AVAudioConverter` can drive it
//  straight into `AVAudioEngine` — no ADTS wrapper is needed for playback. (The crate's
//  `glassesAdtsHeader(frameLen:)` is for the other job: writing a playable `.aac` FILE.)
//
//  Everything here is best-effort by construction. `LiveStreamSession` never lets a failure
//  in this file touch the video path: the audio SETUP can be refused, the socket can fail to
//  bind, the engine can refuse to start, and the live view keeps running silently.
//
//  THREADING: `play(_:)` is called from the RTP receive queue. It converts there and
//  schedules on the player node, which is thread-safe.
//

import AVFoundation
import Foundation

final class LiveAudioPlayer: @unchecked Sendable {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private var converter: AVAudioConverter?
    private var inputFormat: AVAudioFormat?
    private var outputFormat: AVAudioFormat?
    private let lock = NSLock()
    private var running = false

    /// An AAC frame is 1024 samples; the decoder wants that as its packet size.
    private static let framesPerPacket: UInt32 = 1024

    /// `sampleRate` and `channels` come from the crate's `glassesAacConfig()`, or from
    /// `glassesAacParseConfig(hex:)` on the SDP's `config=` parameter — never from a
    /// constant typed here.
    func start(sampleRate: Double, channels: UInt8) throws {
        lock.lock(); defer { lock.unlock() }
        guard !running else { return }

        // Route to the speaker rather than the receiver, and mix rather than take the
        // session over — the glasses may be playing their own audio at the same time.
        let audioSession = AVAudioSession.sharedInstance()
        try audioSession.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        try audioSession.setActive(true)

        var description = AudioStreamBasicDescription(
            mSampleRate: sampleRate,
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0,
            mBytesPerPacket: 0,                   // variable
            mFramesPerPacket: Self.framesPerPacket,
            mBytesPerFrame: 0,
            mChannelsPerFrame: UInt32(max(channels, 1)),
            mBitsPerChannel: 0,
            mReserved: 0
        )
        guard let input = AVAudioFormat(streamDescription: &description) else {
            throw PlayerError.unsupportedInput
        }
        guard let output = AVAudioFormat(
            standardFormatWithSampleRate: sampleRate,
            channels: AVAudioChannelCount(max(channels, 1))
        ) else {
            throw PlayerError.unsupportedOutput
        }

        converter = AVAudioConverter(from: input, to: output)
        inputFormat = input
        outputFormat = output

        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: output)
        try engine.start()
        player.play()
        running = true
    }

    func stop() {
        lock.lock(); defer { lock.unlock() }
        guard running else { return }
        running = false
        player.stop()
        engine.stop()
        engine.detach(player)
        converter = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    /// Decode and schedule one AAC frame, exactly as the depacketizer handed it over.
    func play(_ frame: Data) {
        lock.lock()
        guard running, let converter, let input = inputFormat, let output = outputFormat else {
            lock.unlock()
            return
        }
        lock.unlock()
        guard !frame.isEmpty else { return }

        let compressed = AVAudioCompressedBuffer(
            format: input,
            packetCapacity: 1,
            maximumPacketSize: frame.count
        )
        compressed.byteLength = UInt32(frame.count)
        compressed.packetCount = 1
        frame.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            memcpy(compressed.data, base, frame.count)
        }
        compressed.packetDescriptions?.pointee = AudioStreamPacketDescription(
            mStartOffset: 0,
            mVariableFramesInPacket: 0,
            mDataByteSize: UInt32(frame.count)
        )

        guard let decoded = AVAudioPCMBuffer(
            pcmFormat: output,
            frameCapacity: Self.framesPerPacket
        ) else { return }

        var supplied = false
        var error: NSError?
        let outcome = converter.convert(to: decoded, error: &error) { _, status in
            // One packet per call: hand the buffer over once, then report end-of-stream so
            // the converter returns instead of blocking for more.
            if supplied {
                status.pointee = .noDataNow
                return nil
            }
            supplied = true
            status.pointee = .haveData
            return compressed
        }
        guard outcome != .error, decoded.frameLength > 0 else { return }
        player.scheduleBuffer(decoded, completionHandler: nil)
    }

    enum PlayerError: LocalizedError {
        case unsupportedInput, unsupportedOutput

        var errorDescription: String? {
            switch self {
            case .unsupportedInput: return "could not describe the AAC input format"
            case .unsupportedOutput: return "could not build a PCM output format"
            }
        }
    }
}
