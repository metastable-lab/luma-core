//
//  LiveStreamSession.swift
//  The live camera view: RTSP over TCP, RTP over UDP, H.264 into CoreMedia.
//
//  The whole chain (PROTOCOL.md §12 and §14):
//
//    1. write `0x67` over BLE      glassesOpenWifi(service: .live, p2p: false)
//    2. wait for the SSID          the `.wifiCredentials` event
//    3. settle, then join          glassesTimingSsidSettleMs(), then NEHotspotConfiguration
//    4. RTSP OPTIONS → DESCRIBE → SETUP → PLAY over TCP :554
//    5. RTP on the client UDP ports → Annex-B access units → CMSampleBuffer
//    6. stop: RTSP TEARDOWN, BLE `0x44`, drop the network
//
//  Not one byte of that conversation is parsed here. `GlassesRtspSession` is the state
//  machine: `pendingRequest()` hands out the bytes to write, `feed()` takes whatever the
//  socket returned and reports typed events, `teardown()` produces the last request.
//  `glassesParseSdp` reads the DESCRIBE body, `GlassesH264Depacketizer` turns RTP packets
//  into whole pictures, and `GlassesAacDepacketizer` does the same for the audio track.
//
//  This file owns three sockets, one queue and the CoreMedia plumbing. Where it does look
//  at bytes — splitting Annex-B into NAL units, rewriting the start codes as AVCC lengths —
//  that is a CONTAINER conversion for VideoToolbox, not protocol work, and even there the
//  start code and NAL type come from `glassesH264StartCode()` and `glassesH264NalType`.
//
//  Decoding is left to `AVSampleBufferDisplayLayer`, which is hardware-backed. A software
//  decode of 1600×1200 at 25 fps drowns in jitter.
//

import AVFoundation
import Combine
import CoreMedia
import Foundation
import Network
import LumaCore

@MainActor
final class LiveStreamSession: ObservableObject {

    /// The bring-up sequence, named step by step. Four waits that can each take seconds,
    /// and telling a slow one from a dead one is most of the debugging.
    enum Step: Int, CaseIterable {
        case openingWifi
        case awaitingSsid
        case joining
        case handshaking
        case streaming

        var title: String {
            switch self {
            case .openingWifi: return "Opening the glasses Wi-Fi"
            case .awaitingSsid: return "Waiting for the network name"
            case .joining: return "Joining the network"
            case .handshaking: return "RTSP handshake"
            case .streaming: return "Live"
            }
        }

        var detail: String {
            switch self {
            case .openingWifi: return "BLE 0x67 — glassesOpenWifi(service: .live)"
            case .awaitingSsid: return "the glasses push 0x25 with the SSID"
            case .joining: return "settle for glassesTimingSsidSettleMs(), then join"
            case .handshaking: return "GlassesRtspSession: OPTIONS → DESCRIBE → SETUP → PLAY"
            case .streaming: return "RTP → GlassesH264Depacketizer → AVSampleBufferDisplayLayer"
            }
        }
    }

    /// What the overlay reads out. Everything here is the depacketizer's own counters plus
    /// a frame clock kept on this side.
    struct Stats: Equatable {
        var frames: UInt64 = 0
        var keyframes: UInt64 = 0
        var droppedFragments: UInt64 = 0
        var unitsClosedWithoutMarker: UInt64 = 0
        var unsupportedPackets: UInt64 = 0
        var fps: Double = 0
        var audioFrames: UInt64 = 0

        /// Access units that closed on a timestamp change rather than the marker bit, plus
        /// fragments whose start packet never arrived: the two shapes packet loss takes.
        var lossText: String {
            "\(droppedFragments) dropped frag · \(unitsClosedWithoutMarker) no-marker · \(unsupportedPackets) unsupported"
        }
    }

    @Published private(set) var step: Step = .openingWifi
    @Published private(set) var status = "starting"
    @Published private(set) var failure: String?
    @Published private(set) var stats = Stats()
    @Published private(set) var dimensions: CMVideoDimensions?
    @Published private(set) var audioState = "not requested"
    @Published private(set) var finished = false

    /// Sample buffers for the display layer. The view subscribes; nothing else does.
    let frames = PassthroughSubject<CMSampleBuffer, Never>()

    private let link: GlassesLink
    private var task: Task<Void, Never>?
    private var ssid: String?
    private var video: RtpReceiver?
    private var audio: RtpReceiver?
    private var audioPlayer: LiveAudioPlayer?
    private var transport: TcpTransport?
    private var rtsp: GlassesRtspSession?
    /// Held here for its lifetime, not just by the receiver's callback: the pipeline owns
    /// the depacketizer's rolling state and the format description, and a stream that lost
    /// it would restart from grey at every keyframe.
    private var pipeline: VideoPipeline?

    /// How long after PLAY we wait for the first picture before calling the stream dead.
    /// The glasses normally push an IDR within a second.
    private static let firstFrameTimeout: TimeInterval = 12

    init(link: GlassesLink) {
        self.link = link
    }

    var isStreaming: Bool { step == .streaming && failure == nil }

    // MARK: - Start

    func start() {
        guard task == nil, !finished else { return }
        task = Task { await run() }
    }

    private func run() async {
        do {
            step = .openingWifi
            status = "writing 0x67…"
            link.clearWifiSSID()
            await link.write("openWifi(live)", glassesOpenWifi(service: .live, p2p: false))

            step = .awaitingSsid
            status = "waiting for the 0x25 push…"
            guard let ssid = await link.awaitSSID() else { throw GlassesWiFi.WiFiError.noSSID }
            self.ssid = ssid

            step = .joining
            status = "\(ssid) — settling \(glassesTimingSsidSettleMs()) ms"
            try await Task.sleep(for: GlassesWiFi.ssidSettle)
            status = "joining \(ssid)…"
            try await GlassesWiFi.join(ssid: ssid)
            // RTSP is on :554, so that is the port worth probing — the live access point
            // serves no HTTP file API at all.
            let host = try await GlassesWiFi.waitForHost(port: 554) { [weak self] seconds in
                self?.status = "joined \(ssid) — waiting for the RTSP server (\(seconds)s)"
            }

            try await stream(host: host)
        } catch is CancellationError {
            status = "stopped"
        } catch {
            failure = error.localizedDescription
            status = "failed"
        }
        task = nil
    }

    // MARK: - The RTSP conversation

    private func stream(host: String) async throws {
        step = .handshaking
        status = "connecting to \(glassesRtspStreamUrl(host: host))"

        // The client ports are the crate's defaults; the session is asked for them back so
        // the SETUP header and the bound socket can never disagree.
        let videoPorts = glassesRtspDefaultVideoPorts()
        let audioPorts = glassesRtspDefaultAudioPorts()
        let session = GlassesRtspSession.withOptions(
            url: glassesRtspStreamUrl(host: host),
            videoRtpPort: videoPorts.first ?? 8712,
            audioRtpPort: audioPorts.first ?? 8714,
            wantAudio: true
        )
        rtsp = session

        let videoReceiver = RtpReceiver(port: session.videoClientPort())
        try videoReceiver.bind()
        video = videoReceiver

        // Audio's socket is bound best-effort. A refused bind simply means no audio; it
        // must never take video down with it.
        let audioReceiver = RtpReceiver(port: session.audioClientPort())
        let audioBound = (try? audioReceiver.bind()) != nil
        if audioBound { audio = audioReceiver }

        let transport = TcpTransport(host: host, port: 554)
        self.transport = transport
        try await transport.start()

        var videoPayloadType: UInt8 = 96
        var audioPayloadType: UInt8?
        var aacFmtp: [FfiGlassesKeyValue] = []

        // The loop the crate documents: pendingRequest → write → read → feed → repeat.
        while !session.isPlaying() {
            try Task.checkCancellation()
            if let request = session.pendingRequest() {
                status = Self.firstLine(of: request)
                try await transport.send(request)
            }
            let chunk = try await transport.receive()
            switch session.feed(bytes: chunk) {
            case let .err(reason):
                throw LiveError.rtsp(reason)
            case let .ok(events):
                for event in events {
                    switch event {
                    case let .described(sdp):
                        for media in sdp.media {
                            if media.kind == "video" { videoPayloadType = media.payloadType }
                            if media.kind == "audio" {
                                audioPayloadType = media.payloadType
                                aacFmtp = media.fmtp
                            }
                        }
                        let names = sdp.media.map { "\($0.kind) pt \($0.payloadType)" }
                        status = "DESCRIBE: \(names.joined(separator: ", "))"
                    case let .setUpRefused(track, code):
                        // Documented and expected for audio; a refused VIDEO setup would
                        // have come back as an error from `feed`, not as this event.
                        if track == .audio { audioState = "refused by the glasses (\(code))" }
                    case .playing:
                        status = "PLAY accepted"
                    default:
                        break
                    }
                }
            }
        }

        // Video first, and unconditionally.
        let pipeline = VideoPipeline()
        self.pipeline = pipeline
        pipeline.onFormat = { [weak self] dimensions in
            Task { @MainActor in self?.dimensions = dimensions }
        }
        pipeline.onSample = { [weak self] sample, snapshot in
            Task { @MainActor in
                guard let self else { return }
                if self.step != .streaming { self.step = .streaming; self.status = "streaming" }
                self.apply(snapshot)
                self.frames.send(sample)
            }
        }
        videoReceiver.expectedPayloadType = videoPayloadType
        videoReceiver.onDatagram = { data in pipeline.push(data) }
        videoReceiver.start()

        // Then audio, and only if everything about it worked.
        if audioBound, let audioPayloadType, audioState == "not requested" {
            startAudio(on: audioReceiver, payloadType: audioPayloadType, fmtp: aacFmtp)
        } else if !audioBound {
            audioState = "no socket"
        }

        // Hold the session open. PLAY succeeding is not proof that video is coming: the
        // image processor can stay dark and the access point can drop, and without this
        // watchdog the screen sat on "RTSP handshake" forever with no way back.
        let deadline = Date().addingTimeInterval(Self.firstFrameTimeout)
        while !Task.isCancelled {
            try await Task.sleep(for: .milliseconds(250))
            if let reason = videoReceiver.failure { throw LiveError.socket(reason) }
            if step == .streaming { continue }
            if Date() >= deadline { throw LiveError.noFirstFrame }
        }
        throw CancellationError()
    }

    /// The AAC track, entirely best-effort. `GlassesAacDepacketizer` unpacks the RTP
    /// payloads into raw AAC frames; `LiveAudioPlayer` decodes them. Any failure at all
    /// leaves the video stream untouched and says so in `audioState`.
    private func startAudio(on receiver: RtpReceiver, payloadType: UInt8, fmtp: [FfiGlassesKeyValue]) {
        // The widths come out of the SDP's `a=fmtp:` line. The server spells two of RFC
        // 3640's three field names in lower case, so the lookup is case-insensitive — the
        // crate's own doc comment on `FfiGlassesSdpMedia.fmtp` says as much.
        func width(_ key: String, default fallback: UInt8) -> UInt8 {
            let hit = fmtp.first { $0.key.lowercased() == key }
            return hit.flatMap { UInt8($0.value) } ?? fallback
        }
        let depacketizer = GlassesAacDepacketizer.withWidths(
            sizeLength: width("sizelength", default: 13),
            indexLength: width("indexlength", default: 3),
            indexDeltaLength: width("indexdeltalength", default: 3)
        )

        // `config=1408` is the AudioSpecificConfig as hex; the crate decodes it, and falls
        // back to the glasses' known 16 kHz mono AAC-LC when the line is absent.
        let configHex = fmtp.first { $0.key.lowercased() == "config" }?.value
        let config = configHex.flatMap { glassesAacParseConfig(hex: $0) } ?? glassesAacConfig()

        let player = LiveAudioPlayer()
        do {
            try player.start(sampleRate: Double(config.sampleRateHz), channels: config.channelConfiguration)
        } catch {
            audioState = "decoder unavailable: \(error.localizedDescription)"
            return
        }
        audioPlayer = player
        audioState = "AAC-LC \(config.sampleRateHz) Hz, \(config.channelConfiguration) ch"

        receiver.expectedPayloadType = payloadType
        receiver.onDatagram = { [weak self] data in
            let units = depacketizer.push(packet: data)
            guard !units.isEmpty else { return }
            units.forEach(player.play)
            let count = depacketizer.frames()
            Task { @MainActor in self?.stats.audioFrames = count }
        }
        receiver.start()
    }

    private func apply(_ snapshot: VideoPipeline.Snapshot) {
        stats.frames = snapshot.stats.accessUnits
        stats.keyframes = snapshot.stats.keyframes
        stats.droppedFragments = snapshot.stats.droppedFragments
        stats.unitsClosedWithoutMarker = snapshot.stats.unitsClosedWithoutMarker
        stats.unsupportedPackets = snapshot.stats.unsupportedPackets
        stats.fps = snapshot.fps
    }

    // MARK: - Stop

    /// TEARDOWN, `0x44`, drop the network. Runs from the button and from `onDisappear`,
    /// so it has to be idempotent.
    func stop() {
        guard !finished else { return }
        finished = true
        task?.cancel()
        task = nil

        video?.stop(); video = nil
        audio?.stop(); audio = nil
        audioPlayer?.stop(); audioPlayer = nil
        pipeline = nil

        let session = rtsp
        let transport = self.transport
        rtsp = nil
        self.transport = nil
        let leaving = ssid
        ssid = nil

        // The step is left where it was on purpose: stopping during the handshake should
        // read as "stopped there", not as a completed checklist.
        status = "TEARDOWN, then 0x44"
        Task {
            if let session, let transport {
                // Best effort: the socket may already be gone with the access point.
                try? await transport.send(session.teardown())
            }
            transport?.cancel()
            session?.close()
            await link.write("fileDownloadComplete", glassesFileDownloadComplete())
            if let leaving { GlassesWiFi.leave(ssid: leaving) }
        }
    }

    func dismissFailure() { failure = nil }

    private static func firstLine(of request: Data) -> String {
        String(decoding: request, as: UTF8.self)
            .split(separator: "\r\n", maxSplits: 1).first.map(String.init) ?? "…"
    }

    enum LiveError: LocalizedError {
        case rtsp(String)
        case socket(String)
        case noFirstFrame

        var errorDescription: String? {
            switch self {
            case let .rtsp(reason): return "RTSP failed: \(reason)"
            case let .socket(reason): return "The video socket failed: \(reason)"
            case .noFirstFrame: return "PLAY was accepted but no video arrived. Try again."
            }
        }
    }
}

// MARK: - The RTSP control socket

/// A TCP connection with `send`/`receive` as plain async calls. RTSP is request/response
/// over a stream, and `GlassesRtspSession.feed` re-assembles whatever chunking the socket
/// happens to produce, so nothing here needs to buffer.
final class TcpTransport: @unchecked Sendable {
    private let connection: NWConnection
    private let queue = DispatchQueue(label: "luma.rtsp")

    init(host: String, port: UInt16) {
        connection = NWConnection(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(rawValue: port) ?? 554,
            using: .tcp
        )
    }

    func start() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let once = ResumeOnce()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    once.run { continuation.resume() }
                case let .failed(error):
                    once.run { continuation.resume(throwing: error) }
                case .cancelled:
                    once.run { continuation.resume(throwing: CancellationError()) }
                default:
                    break
                }
            }
            connection.start(queue: queue)
        }
    }

    func send(_ data: Data) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let once = ResumeOnce()
            connection.send(content: data, completion: .contentProcessed { error in
                once.run {
                    if let error { continuation.resume(throwing: error) } else { continuation.resume() }
                }
            })
        }
    }

    func receive() async throws -> Data {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Data, Error>) in
            let once = ResumeOnce()
            connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { data, _, isComplete, error in
                once.run {
                    if let error { continuation.resume(throwing: error); return }
                    if let data, !data.isEmpty { continuation.resume(returning: data); return }
                    if isComplete {
                        continuation.resume(throwing: LiveStreamSession.LiveError.socket("the server closed the connection"))
                    } else {
                        continuation.resume(returning: Data())
                    }
                }
            }
        }
    }

    func cancel() { connection.cancel() }
}

// MARK: - The RTP sockets

/// Binds one UDP port and hands whole datagrams to `onDatagram` on its own queue.
///
/// Datagrams whose RTP payload type is not the expected one are dropped: RTCP shares the
/// neighbourhood and the access point is not private. `glassesParseRtp` reads the header —
/// there is no RTP parsing in this file.
final class RtpReceiver: @unchecked Sendable {
    private var listener: NWListener?
    private var connections: [NWConnection] = []
    private let queue = DispatchQueue(label: "luma.rtp", qos: .userInitiated)
    private let port: UInt16

    var expectedPayloadType: UInt8?
    var onDatagram: ((Data) -> Void)?

    private let lock = NSLock()
    private var failureReason: String?
    private var stopped = false

    var failure: String? {
        lock.lock(); defer { lock.unlock() }
        return failureReason
    }

    init(port: UInt16) { self.port = port }

    /// The initializer only fails on bad parameters; a port already in use surfaces later
    /// as `.failed` on the state handler, which is why one is installed here. With no
    /// handler that failure is invisible: the socket looks bound and nothing ever arrives.
    func bind() throws {
        let parameters = NWParameters.udp
        parameters.allowLocalEndpointReuse = true
        let listener = try NWListener(using: parameters, on: NWEndpoint.Port(rawValue: port) ?? .any)
        listener.stateUpdateHandler = { [weak self] state in
            switch state {
            case let .failed(error): self?.record("listener failed: \(error.localizedDescription)")
            case .cancelled: self?.record("listener cancelled")
            default: break
            }
        }
        self.listener = listener
    }

    func start() {
        listener?.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            self.connections.append(connection)
            connection.start(queue: self.queue)
            self.receive(on: connection)
        }
        listener?.start(queue: queue)
    }

    private func receive(on connection: NWConnection) {
        connection.receiveMessage { [weak self] data, _, _, error in
            guard let self else { return }
            if let data, !data.isEmpty, self.accepts(data) { self.onDatagram?(data) }
            if let error {
                self.record("receive failed: \(error.localizedDescription)")
                return
            }
            self.receive(on: connection)
        }
    }

    private func accepts(_ datagram: Data) -> Bool {
        guard let expected = expectedPayloadType else { return true }
        guard case let .ok(header) = glassesParseRtp(packet: datagram) else { return false }
        return header.payloadType == expected
    }

    private func record(_ reason: String) {
        lock.lock(); defer { lock.unlock() }
        guard !stopped, failureReason == nil else { return }
        failureReason = reason
    }

    func stop() {
        lock.lock(); stopped = true; lock.unlock()
        listener?.cancel()
        listener = nil
        connections.forEach { $0.cancel() }
        connections.removeAll()
        onDatagram = nil
    }
}

// MARK: - RTP → CMSampleBuffer

/// Confined to the RTP receive queue. Owns the depacketizer, the format description and
/// the frame clock; publishes finished sample buffers through `onSample`.
final class VideoPipeline: @unchecked Sendable {
    struct Snapshot {
        let stats: FfiGlassesH264Stats
        let fps: Double
    }

    var onSample: ((CMSampleBuffer, Snapshot) -> Void)?
    var onFormat: ((CMVideoDimensions) -> Void)?

    private let depacketizer = GlassesH264Depacketizer()
    private var formatDescription: CMFormatDescription?
    private var windowStart = Date()
    private var windowFrames = 0
    private var fps: Double = 0

    /// The crate's own start code. Every access unit it emits is `00 00 00 01 <nal>`
    /// repeated, so this is both the separator and the prefix width.
    private let startCode = glassesH264StartCode()

    func push(_ datagram: Data) {
        for unit in depacketizer.pushDetailed(packet: datagram) {
            // A decoder handed anything before the first IDR-with-parameter-sets renders
            // grey until the next keyframe, which reads as a decoder bug and is not one.
            if formatDescription == nil {
                guard unit.isDecodableStart else { continue }
                makeFormatDescription()
            }
            guard let sample = makeSampleBuffer(unit) else { continue }
            tickFrameClock()
            onSample?(sample, Snapshot(stats: depacketizer.stats(), fps: fps))
        }
    }

    /// SPS and PPS arrive in-band before every IDR; `parameterSets()` hands back the most
    /// recent pair as Annex B, which splits into exactly the two NALs VideoToolbox wants.
    private func makeFormatDescription() {
        let nals = splitAnnexB(depacketizer.parameterSets())
        let sps = nals.first { glassesH264NalType(headerByte: $0.first ?? 0) == 7 }
        let pps = nals.first { glassesH264NalType(headerByte: $0.first ?? 0) == 8 }
        guard let sps, let pps else { return }

        var format: CMFormatDescription?
        let status = sps.withUnsafeBytes { spsBuffer in
            pps.withUnsafeBytes { ppsBuffer -> OSStatus in
                guard let s = spsBuffer.bindMemory(to: UInt8.self).baseAddress,
                      let p = ppsBuffer.bindMemory(to: UInt8.self).baseAddress else { return -1 }
                var pointers = [s, p]
                var sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: &pointers,
                    parameterSetSizes: &sizes,
                    nalUnitHeaderLength: 4,          // AVCC, 4-byte length prefix
                    formatDescriptionOut: &format
                )
            }
        }
        guard status == noErr, let format else { return }
        formatDescription = format
        onFormat?(CMVideoFormatDescriptionGetDimensions(format))
    }

    /// Annex B in, AVCC out: each `00 00 00 01` start code becomes a 4-byte big-endian
    /// length. SPS and PPS are dropped from the sample itself — they are already in the
    /// format description, and VideoToolbox wants slices here.
    private func makeSampleBuffer(_ unit: FfiGlassesAccessUnit) -> CMSampleBuffer? {
        guard let format = formatDescription else { return nil }
        var avcc = Data()
        for nal in splitAnnexB(unit.data) {
            let type = glassesH264NalType(headerByte: nal.first ?? 0)
            if type == 7 || type == 8 { continue }
            var length = UInt32(nal.count).bigEndian
            withUnsafeBytes(of: &length) { avcc.append(contentsOf: $0) }
            avcc.append(nal)
        }
        guard !avcc.isEmpty else { return nil }

        // CoreMedia owns the storage and we copy into it — pointing a block buffer at a
        // `Data`'s buffer dangles the moment this scope ends.
        var block: CMBlockBuffer?
        let length = avcc.count
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: length,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: length,
            flags: kCMBlockBufferAssureMemoryNowFlag,
            blockBufferOut: &block
        ) == noErr, let block else { return nil }

        let copied = avcc.withUnsafeBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(
                with: base, blockBuffer: block, offsetIntoDestination: 0, dataLength: length
            )
        }
        guard copied == noErr else { return nil }

        var sample: CMSampleBuffer?
        var sizes = [length]
        guard CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: block,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 0,
            sampleTimingArray: nil,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sizes,
            sampleBufferOut: &sample
        ) == noErr, let sample else { return nil }

        // The SDP signals no frame rate (PROTOCOL.md §14), so there is no honest
        // presentation timestamp to compute: display each picture as it arrives.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true) {
            let dictionary = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dictionary,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque()
            )
            if !unit.isKeyframe {
                CFDictionarySetValue(
                    dictionary,
                    Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque(),
                    Unmanaged.passUnretained(kCFBooleanTrue).toOpaque()
                )
            }
        }
        return sample
    }

    private func tickFrameClock() {
        windowFrames += 1
        let elapsed = Date().timeIntervalSince(windowStart)
        if elapsed >= 1 {
            fps = Double(windowFrames) / elapsed
            windowFrames = 0
            windowStart = Date()
        }
    }

    /// Split `00 00 00 01`-delimited bytes into NAL units. A container detail, not protocol:
    /// the separator itself is `glassesH264StartCode()`.
    private func splitAnnexB(_ data: Data) -> [Data] {
        let code = [UInt8](startCode)
        guard !code.isEmpty, data.count > code.count else { return [] }
        let bytes = [UInt8](data)
        var starts: [Int] = []
        var i = 0
        while i + code.count <= bytes.count {
            if Array(bytes[i..<(i + code.count)]) == code {
                starts.append(i + code.count)
                i += code.count
            } else {
                i += 1
            }
        }
        return starts.enumerated().compactMap { index, start in
            let end = index + 1 < starts.count ? starts[index + 1] - code.count : bytes.count
            guard end > start else { return nil }
            return Data(bytes[start..<end])
        }
    }
}
