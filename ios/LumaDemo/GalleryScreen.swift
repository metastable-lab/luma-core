//
//  GalleryScreen.swift
//  Browse, download and delete the files on the glasses, over their own Wi-Fi.
//
//  The whole flow, spelled out on screen as it happens, because reading it happen is the
//  point of the demo:
//
//    1. write `0x39` over BLE          glassesOpenWifi(service: .files, p2p: false)
//    2. wait for the SSID              the `.wifiCredentials` event, pushed ~2.5 s later
//    3. wait for the AP to settle      glassesTimingSsidSettleMs()
//    4. join the network               NEHotspotConfiguration, passphrase from the crate
//    5. list / thumbnail / download / delete   FileApiClient, all URLs from the crate
//    6. "Done" writes `0x44`           glassesFileDownloadComplete(), then leave the network
//
//  Step 6 matters even when nothing was downloaded. Without the `0x44` the glasses sit in
//  SoftAP mode with the image processor powered and flatten the battery. It runs on every
//  exit path here: the button, a swipe back, and a failure mid-flow.
//

import Combine
import SwiftUI
import LumaCore

@MainActor
final class GalleryModel: ObservableObject {

    /// The bring-up sequence. Four separate waits, each of which can take seconds, so each
    /// one is named: a single "Loading…" makes a slow join look identical to a dead one.
    enum Step: Int, CaseIterable {
        case openingWifi
        case awaitingSsid
        case settling
        case joining
        case listing
        case browsing

        var title: String {
            switch self {
            case .openingWifi: return "Opening the glasses Wi-Fi"
            case .awaitingSsid: return "Waiting for the network name"
            case .settling: return "Letting the access point come up"
            case .joining: return "Joining the network"
            case .listing: return "Reading the file list"
            case .browsing: return "Ready"
            }
        }

        var detail: String {
            switch self {
            case .openingWifi:
                return "BLE 0x39 — glassesOpenWifi(service: .files)"
            case .awaitingSsid:
                return "the glasses push 0x25 with the SSID about 2.5 s later"
            case .settling:
                return "glassesTimingSsidSettleMs() — joining sooner fails association"
            case .joining:
                return "NEHotspotConfiguration, passphrase from glassesWifiPassphrase()"
            case .listing:
                return "GET glassesWifiListUrl(), read by glassesWifiParseFileList"
            case .browsing:
                return "tap a row to download, swipe to delete"
            }
        }
    }

    @Published private(set) var step: Step = .openingWifi
    @Published private(set) var status: String = "starting"
    @Published private(set) var failure: String?
    @Published private(set) var sections: [GallerySection] = []
    @Published private(set) var summary: String = ""
    @Published private(set) var thumbnails: [String: Data] = [:]
    @Published private(set) var busyItem: String?
    @Published private(set) var savedFiles: [URL] = []
    @Published private(set) var finished = false

    private let link: GlassesLink
    private var session: URLSession?
    private var client: FileApiClient?
    private var ssid: String?
    private var task: Task<Void, Never>?

    /// The file API is plain HTTP, so reachability is a socket on port 80.
    private static let httpPort: UInt16 = 80

    init(link: GlassesLink) {
        self.link = link
    }

    var isBrowsing: Bool { step == .browsing && failure == nil }

    // MARK: - The flow

    func start() {
        guard task == nil, !finished else { return }
        task = Task { await run() }
    }

    private func run() async {
        do {
            // 1. Open the access point for the FILE API. `0x67` opens the same network for
            //    the live stream instead — same SSID, different server, so the choice is the
            //    `FfiGlassesWifiService` case and nothing else.
            step = .openingWifi
            status = "writing 0x39…"
            link.clearWifiSSID()
            await link.write("openWifi(files)", glassesOpenWifi(service: .files, p2p: false))

            // 2. The SSID comes back over BLE, not over Wi-Fi.
            step = .awaitingSsid
            status = "waiting for the 0x25 push…"
            guard let ssid = await link.awaitSSID() else {
                throw GlassesWiFi.WiFiError.noSSID
            }
            self.ssid = ssid

            // 3. The radio is not associable the instant the name arrives.
            step = .settling
            let settleMs = glassesTimingSsidSettleMs()
            status = "\(ssid) — waiting \(settleMs) ms for the AP"
            try await Task.sleep(for: GlassesWiFi.ssidSettle)

            // 4. One join, then poll the fixed host until it answers. Whether iOS or the
            //    user gets us onto the network, the probe is what proves we are on it.
            step = .joining
            status = "joining \(ssid)…"
            try await GlassesWiFi.join(ssid: ssid)
            let host = try await GlassesWiFi.waitForHost(port: Self.httpPort) { [weak self] seconds in
                self?.status = "joined \(ssid) — waiting for the file server (\(seconds)s)"
            }

            // 5. From here it is four plain GETs.
            step = .listing
            status = "GET \(glassesWifiListUrl())"
            let session = GlassesWiFi.makeSession()
            self.session = session
            let client = FileApiClient(session: session)
            self.client = client

            let listing = try await client.list()
            sections = listing.sections
            summary = "\(listing.totalFiles) files, \(listing.totalKib) KB on \(host)"
            step = .browsing
            status = "ready"
            await loadThumbnails()
        } catch is CancellationError {
            status = "cancelled"
        } catch {
            failure = error.localizedDescription
            status = "failed"
        }
        task = nil
    }

    /// Refetch the listing without re-joining — used after a delete.
    func refresh() async {
        guard let client else { return }
        do {
            let listing = try await client.list()
            sections = listing.sections
            summary = "\(listing.totalFiles) files, \(listing.totalKib) KB"
            await loadThumbnails()
        } catch {
            failure = error.localizedDescription
        }
    }

    /// Thumbnails are best effort and deliberately serial: the glasses' HTTP server is a
    /// small embedded one, and a dozen parallel GETs on a fresh access point is how a
    /// listing turns into a stall.
    private func loadThumbnails() async {
        guard let client else { return }
        for section in sections {
            for item in section.items where thumbnails[item.id] == nil && item.hasThumbnail {
                if Task.isCancelled { return }
                if let data = await client.thumbnail(for: item) {
                    thumbnails[item.id] = data
                }
            }
        }
    }

    // MARK: - Per-file actions

    func download(_ item: GalleryItem) {
        guard let client, busyItem == nil else { return }
        busyItem = item.id
        Task {
            defer { busyItem = nil }
            do {
                status = "GET \(glassesWifiDownloadUrl(name: item.entry.name))"
                let url = try await client.download(item)
                savedFiles.removeAll { $0.lastPathComponent == url.lastPathComponent }
                savedFiles.insert(url, at: 0)
                status = "saved \(url.lastPathComponent) to Documents"
            } catch {
                failure = error.localizedDescription
                status = "download failed"
            }
        }
    }

    func delete(_ item: GalleryItem) {
        guard let client, busyItem == nil else { return }
        busyItem = item.id
        Task {
            defer { busyItem = nil }
            do {
                status = "GET \(glassesWifiDeleteUrl(name: item.entry.name))"
                try await client.delete(item)
                thumbnails[item.id] = nil
                status = "deleted \(item.basename)"
                await refresh()
            } catch {
                failure = error.localizedDescription
                status = "delete failed"
            }
        }
    }

    // MARK: - Teardown

    /// `0x44 30 00` — "all done". The glasses power the image processor down and drop the
    /// access point; then the hotspot configuration goes so the phone returns to normal
    /// Wi-Fi. Idempotent, because it runs from both the button and `onDisappear`.
    func finish() {
        guard !finished else { return }
        finished = true
        task?.cancel()
        task = nil
        session?.finishTasksAndInvalidate()
        session = nil
        client = nil
        let leaving = ssid
        ssid = nil
        status = "writing 0x44 and leaving the network"
        Task {
            await link.write("fileDownloadComplete", glassesFileDownloadComplete())
            if let leaving { GlassesWiFi.leave(ssid: leaving) }
        }
    }

    func dismissFailure() { failure = nil }
}

// MARK: - The screen

struct GalleryScreen: View {
    @StateObject private var model: GalleryModel
    @Environment(\.dismiss) private var dismiss

    init(link: GlassesLink) {
        _model = StateObject(wrappedValue: GalleryModel(link: link))
    }

    var body: some View {
        List {
            Section("Flow") { FlowChecklist(model: model) }

            if let failure = model.failure {
                Section {
                    Text(failure).font(.footnote).foregroundStyle(.red)
                    Button("Dismiss") { model.dismissFailure() }
                }
            }

            if model.isBrowsing {
                ForEach(model.sections) { section in
                    Section {
                        if section.items.isEmpty {
                            Text("empty").font(.caption).foregroundStyle(.secondary)
                        }
                        ForEach(section.items) { item in
                            FileRow(
                                item: item,
                                thumbnail: model.thumbnails[item.id],
                                busy: model.busyItem == item.id
                            )
                            .contentShape(Rectangle())
                            .onTapGesture { model.download(item) }
                            .swipeActions {
                                Button("Delete", role: .destructive) { model.delete(item) }
                            }
                        }
                    } header: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(section.title)
                            Text(section.subtitle)
                                .font(.caption2)
                                .textCase(nil)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }

            if !model.savedFiles.isEmpty {
                Section("Saved to Documents") {
                    ForEach(model.savedFiles, id: \.self) { url in
                        ShareLink(item: url) {
                            Label(url.lastPathComponent, systemImage: "square.and.arrow.up")
                                .font(.callout)
                        }
                    }
                }
            }
        }
        .navigationTitle("Gallery")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button("Done") {
                    model.finish()
                    dismiss()
                }
            }
        }
        .onAppear { model.start() }
        // Leaving by any route — the button, the back swipe, a tab switch — must still send
        // 0x44 and drop the network, or the glasses stay in SoftAP mode.
        .onDisappear { model.finish() }
    }
}

/// The bring-up sequence, with the step currently running marked. Each row names the
/// binding or the frame doing the work, so the screen doubles as the documentation.
private struct FlowChecklist: View {
    @ObservedObject var model: GalleryModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(GalleryModel.Step.allCases, id: \.rawValue) { step in
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: marker(step))
                        .foregroundStyle(colour(step))
                        .font(.caption)
                        .frame(width: 16)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(step.title)
                            .font(.caption)
                            .foregroundStyle(step.rawValue <= model.step.rawValue ? .primary : .secondary)
                        Text(step.detail)
                            .font(.system(size: 10, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Divider()
            Text(model.status)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
            if !model.summary.isEmpty {
                Text(model.summary).font(.caption2).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }

    private func marker(_ step: GalleryModel.Step) -> String {
        if model.failure != nil, step == model.step { return "xmark.circle" }
        if step.rawValue < model.step.rawValue { return "checkmark.circle.fill" }
        if step == model.step { return "circle.dotted" }
        return "circle"
    }

    private func colour(_ step: GalleryModel.Step) -> Color {
        if model.failure != nil, step == model.step { return .red }
        return step.rawValue < model.step.rawValue ? .green : .secondary
    }
}

private struct FileRow: View {
    let item: GalleryItem
    let thumbnail: Data?
    let busy: Bool

    var body: some View {
        HStack(spacing: 10) {
            ZStack {
                RoundedRectangle(cornerRadius: 6).fill(.quaternary)
                if let thumbnail, let image = UIImage(data: thumbnail) {
                    Image(uiImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                } else {
                    Image(systemName: item.systemImage).foregroundStyle(.secondary)
                }
            }
            .frame(width: 52, height: 40)
            .clipShape(RoundedRectangle(cornerRadius: 6))

            VStack(alignment: .leading, spacing: 2) {
                Text(item.basename).font(.callout).lineLimit(1)
                Text("\(item.sizeText) · \(item.createdText)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if busy {
                ProgressView()
            } else {
                Image(systemName: "arrow.down.circle").foregroundStyle(.secondary)
            }
        }
    }
}
