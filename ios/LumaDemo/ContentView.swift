//
//  ContentView.swift
//  One screen: status header, discovered devices, event log, two command buttons.
//

import CoreBluetooth
import SwiftUI

struct ContentView: View {
    @StateObject private var link = GlassesLink()

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                StatusHeader(status: link.status)
                Divider()

                if link.status.isConnected {
                    CommandBar(link: link)
                    Divider()
                } else {
                    DeviceList(link: link)
                    Divider()
                }

                EventLog(lines: link.log)
            }
            .navigationTitle("Glasses")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    if link.status.isConnected {
                        Button("Disconnect", role: .destructive) { link.disconnect() }
                    } else if link.isScanning {
                        Button("Stop") { link.stopScan() }
                    } else {
                        Button("Scan") { link.startScan() }
                    }
                }
            }
        }
    }
}

private struct StatusHeader: View {
    let status: LinkStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Circle()
                    .fill(status.isConnected ? .green : .secondary)
                    .frame(width: 8, height: 8)
                Text(status.deviceName ?? "no device")
                    .font(.headline)
                Spacer()
                Text(status.phase)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 14) {
                Label(
                    status.batteryPercent.map { "\($0)%\(status.charging ? " ⚡" : "")" } ?? "—",
                    systemImage: "battery.50"
                )
                if status.settingsComplete {
                    Label("settings synced", systemImage: "checkmark.circle")
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)

            if let firmware = status.firmware {
                Text(firmware).font(.caption2.monospaced()).foregroundStyle(.secondary)
            }
            if let project = status.project {
                Text("project \(project)").font(.caption2.monospaced()).foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding()
    }
}

private struct DeviceList: View {
    @ObservedObject var link: GlassesLink

    var body: some View {
        Group {
            if link.found.isEmpty {
                VStack(spacing: 4) {
                    Text(link.isScanning ? "scanning…" : "tap Scan")
                    Text("advertising service AA12")
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 18)
            } else {
                ForEach(link.found, id: \.identifier) { p in
                    Button { link.connect(p) } label: {
                        HStack {
                            VStack(alignment: .leading) {
                                Text(p.name ?? "(unnamed)")
                                Text(p.identifier.uuidString)
                                    .font(.caption2.monospaced())
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            Image(systemName: "chevron.right").foregroundStyle(.secondary)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .padding(.horizontal)
                    .padding(.vertical, 8)
                }
            }
        }
    }
}

private struct CommandBar: View {
    let link: GlassesLink

    var body: some View {
        HStack(spacing: 12) {
            Button("Take photo") { link.takePhoto() }
                .buttonStyle(.borderedProminent)
            // 0x56 is the ONLY thing that closes the mic on this firmware.
            Button("Interrupt voice") { link.interruptVoice() }
                .buttonStyle(.bordered)
            Spacer()
        }
        .padding(.horizontal)
        .padding(.vertical, 10)
    }
}

private struct EventLog: View {
    let lines: [LogLine]

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 3) {
                    ForEach(lines) { line in
                        HStack(alignment: .top, spacing: 6) {
                            Text(marker(line.direction))
                                .foregroundStyle(colour(line.direction))
                            Text(line.text)
                                .textSelection(.enabled)
                        }
                        .font(.system(size: 11, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .id(line.id)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
            }
            .onChange(of: lines.count) {
                if let last = lines.last { proxy.scrollTo(last.id, anchor: .bottom) }
            }
        }
    }

    private func marker(_ d: LogLine.Direction) -> String {
        switch d {
        case .out: "→"
        case .incoming: "←"
        case .note: "·"
        }
    }

    private func colour(_ d: LogLine.Direction) -> Color {
        switch d {
        case .out: .blue
        case .incoming: .green
        case .note: .secondary
        }
    }
}

#Preview {
    ContentView()
}
