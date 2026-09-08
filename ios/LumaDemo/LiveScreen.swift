//
//  LiveScreen.swift
//  The glasses' camera, live, with the bring-up sequence and the depacketizer's counters
//  on screen beside it.
//
//  The view layer is thin on purpose. `LiveStreamSession` does the whole §12/§14 dance and
//  publishes finished `CMSampleBuffer`s; this file enqueues them on an
//  `AVSampleBufferDisplayLayer` (hardware decode, via `AVSampleBufferVideoRenderer`) and
//  draws the readouts. No protocol byte reaches this file at all.
//
//  The counters are worth watching while it runs. `fps` is measured on this side, because
//  the SDP signals no frame rate (PROTOCOL.md §14); the loss figures come straight from
//  `GlassesH264Depacketizer.stats()` and are the honest measure of a marginal link:
//  `droppedFragments` are FU-A continuations whose start packet never arrived, and
//  `unitsClosedWithoutMarker` are pictures that ended because the timestamp moved on rather
//  than because the marker bit said so.
//

import AVFoundation
import Combine
import CoreMedia
import SwiftUI
import LumaCore

struct LiveScreen: View {
    @StateObject private var session: LiveStreamSession
    @Environment(\.dismiss) private var dismiss

    init(link: GlassesLink) {
        _session = StateObject(wrappedValue: LiveStreamSession(link: link))
    }

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color.black
                LiveVideoView(frames: session.frames)
                if !session.isStreaming {
                    VStack(spacing: 6) {
                        ProgressView().tint(.white)
                        Text(session.step.title).font(.callout).foregroundStyle(.white)
                        Text(session.status)
                            .font(.system(size: 10, design: .monospaced))
                            .foregroundStyle(.white.opacity(0.7))
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 24)
                    }
                }
                if session.isStreaming {
                    VStack {
                        Spacer()
                        StatsOverlay(session: session)
                    }
                }
            }
            .frame(maxWidth: .infinity)
            .aspectRatio(4.0 / 3.0, contentMode: .fit)   // 1600 × 1200
            .clipped()

            Divider()

            List {
                Section("Flow") { LiveChecklist(session: session) }

                if let failure = session.failure {
                    Section {
                        Text(failure).font(.footnote).foregroundStyle(.red)
                        Button("Dismiss") { session.dismissFailure() }
                    }
                }

                Section("Stream") {
                    Row("video", session.dimensions.map { "\($0.width) × \($0.height) H.264" } ?? "—")
                    Row("frames", "\(session.stats.frames) (\(session.stats.keyframes) key)")
                    Row("fps", String(format: "%.1f", session.stats.fps))
                    Row("loss", session.stats.lossText)
                    Row("audio", session.audioState)
                    if session.stats.audioFrames > 0 {
                        Row("aac frames", "\(session.stats.audioFrames)")
                    }
                }
            }
            .listStyle(.plain)
        }
        .navigationTitle("Live view")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button("Stop", role: .destructive) {
                    session.stop()
                    dismiss()
                }
            }
        }
        .onAppear { session.start() }
        // TEARDOWN + 0x44 + leave the network, whichever way the screen is left. Without
        // it the glasses stay in access-point mode with the camera powered.
        .onDisappear { session.stop() }
    }

    private func Row(_ label: String, _ value: String) -> some View {
        HStack {
            Text(label).font(.caption).foregroundStyle(.secondary)
            Spacer()
            Text(value).font(.caption.monospaced()).multilineTextAlignment(.trailing)
        }
    }
}

// MARK: - The display layer

/// A `UIViewRepresentable` around `AVSampleBufferDisplayLayer`.
///
/// The layer is the decoder: hand it sample buffers whose format description was built from
/// the stream's own SPS and PPS and it decodes in hardware. Everything upstream of here has
/// already been done by the crate and `VideoPipeline`.
struct LiveVideoView: UIViewRepresentable {
    let frames: PassthroughSubject<CMSampleBuffer, Never>

    func makeUIView(context: Context) -> SampleBufferView {
        let view = SampleBufferView()
        context.coordinator.attach(to: view, frames: frames)
        return view
    }

    func updateUIView(_ uiView: SampleBufferView, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        private var cancellable: AnyCancellable?

        func attach(to view: SampleBufferView, frames: PassthroughSubject<CMSampleBuffer, Never>) {
            cancellable = frames.sink { [weak view] sample in
                view?.enqueue(sample)
            }
        }
    }

    /// A `UIView` whose backing layer IS the display layer, so there is no second layer to
    /// keep in sync with the view's bounds.
    final class SampleBufferView: UIView {
        override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }

        private var displayLayer: AVSampleBufferDisplayLayer {
            layer as! AVSampleBufferDisplayLayer
        }

        override init(frame: CGRect) {
            super.init(frame: frame)
            backgroundColor = .black
            displayLayer.videoGravity = .resizeAspect
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not used") }

        func enqueue(_ sample: CMSampleBuffer) {
            let renderer = displayLayer.sampleBufferRenderer
            // A decode failure latches; flushing clears it so the next keyframe can start
            // the picture again rather than the view staying frozen for good.
            if renderer.status == .failed { renderer.flush() }
            renderer.enqueue(sample)
        }
    }
}

// MARK: - Readouts

private struct StatsOverlay: View {
    @ObservedObject var session: LiveStreamSession

    var body: some View {
        HStack(spacing: 12) {
            Label(String(format: "%.0f fps", session.stats.fps), systemImage: "speedometer")
            if let dimensions = session.dimensions {
                Text("\(dimensions.width)×\(dimensions.height)")
            }
            Text("\(session.stats.frames) frames")
            Spacer()
        }
        .font(.system(size: 10, design: .monospaced))
        .foregroundStyle(.white)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(.black.opacity(0.45))
    }
}

/// The same checklist idea as the Gallery screen: every step named, with the binding or the
/// frame that does the work under it, so the screen doubles as the documentation.
private struct LiveChecklist: View {
    @ObservedObject var session: LiveStreamSession

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(LiveStreamSession.Step.allCases, id: \.rawValue) { step in
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: marker(step))
                        .foregroundStyle(colour(step))
                        .font(.caption)
                        .frame(width: 16)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(step.title)
                            .font(.caption)
                            .foregroundStyle(step.rawValue <= session.step.rawValue ? .primary : .secondary)
                        Text(step.detail)
                            .font(.system(size: 10, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Divider()
            Text(session.status)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
        }
        .padding(.vertical, 2)
    }

    private func marker(_ step: LiveStreamSession.Step) -> String {
        if session.failure != nil, step == session.step { return "xmark.circle" }
        if step.rawValue < session.step.rawValue { return "checkmark.circle.fill" }
        if step == session.step { return "circle.dotted" }
        return "circle"
    }

    private func colour(_ step: LiveStreamSession.Step) -> Color {
        if session.failure != nil, step == session.step { return .red }
        return step.rawValue < session.step.rawValue ? .green : .secondary
    }
}
