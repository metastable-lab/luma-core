//
//  GlassesLink.swift
//  The whole radio. Everything protocol-shaped comes from `LumaCore`.
//
//  This file owns CoreBluetooth and nothing else: no UUID is typed here (they come from
//  `glassesGatt()`), no frame is built here (the `glasses*` builders do that), and no byte
//  is interpreted here (`GlassesParser` does that). If you find yourself adding a byte
//  offset to this file, it belongs in the Rust crate instead.
//

import Combine
import CoreBluetooth
import Foundation
import LumaCore

/// One line in the event log.
struct LogLine: Identifiable, Equatable {
    enum Direction { case out, incoming, note }

    let id = UUID()
    let at = Date()
    let direction: Direction
    let text: String
}

/// Everything the header renders. Written on the main actor only.
struct LinkStatus: Equatable {
    var phase: String = "idle"
    var deviceName: String?
    var batteryPercent: UInt8?
    var charging = false
    var firmware: String?
    var project: String?
    var settingsComplete = false
    var isConnected: Bool { phase == "connected" }
}

@MainActor
final class GlassesLink: NSObject, ObservableObject {
    @Published private(set) var found: [CBPeripheral] = []
    @Published private(set) var log: [LogLine] = []
    @Published private(set) var status = LinkStatus()
    @Published private(set) var isScanning = false

    /// The SSID from the most recent `.wifiCredentials` event (`0x25`), or nil if the glasses
    /// have not announced one since it was last cleared. The two Wi-Fi screens clear it
    /// before they write `0x39`/`0x67`, so they can never join yesterday's network.
    @Published private(set) var wifiSSID: String?

    /// Every parsed event, as it arrives. The Gallery and Live screens subscribe rather than
    /// reach for a peripheral, which keeps this the single CoreBluetooth owner.
    let events = PassthroughSubject<FfiGlassesEvent, Never>()

    /// The one place the GATT topology is read. Short 16-bit UUIDs — `CBUUID` expands them
    /// against the SIG base itself, so no string surgery is needed here.
    private let gatt = glassesGatt()
    private lazy var serviceUUID = CBUUID(string: gatt.service)
    private lazy var writeUUID = CBUUID(string: gatt.write)
    private lazy var notifyUUIDs = gatt.notifyAll.map { CBUUID(string: $0) }

    private var central: CBCentralManager!
    private var peripheral: CBPeripheral?
    private var writeCharacteristic: CBCharacteristic?

    /// **One parser per characteristic.** `AA14` and `AA15` are independent byte streams;
    /// sharing a deframer between them interleaves two half-frames into one corrupt one.
    private var parsers: [CBUUID: GlassesParser] = [:]

    override init() {
        super.init()
        central = CBCentralManager(delegate: self, queue: .main)
    }

    // MARK: - Commands the UI can fire

    func startScan() {
        guard central.state == .poweredOn else {
            note("bluetooth is \(stateLabel(central.state)) — cannot scan")
            return
        }
        found.removeAll()
        isScanning = true
        status.phase = "scanning"
        note("scanning for service \(gatt.service)")
        central.scanForPeripherals(withServices: [serviceUUID])
    }

    func stopScan() {
        central.stopScan()
        isScanning = false
        if peripheral == nil { status.phase = "idle" }
    }

    func connect(_ p: CBPeripheral) {
        stopScan()
        peripheral = p
        p.delegate = self
        status.phase = "connecting"
        status.deviceName = p.name
        note("connecting to \(p.name ?? p.identifier.uuidString)")
        central.connect(p)
    }

    func disconnect() {
        guard let p = peripheral else { return }
        // `0x56` is the only thing that closes the mic on this firmware — the device never
        // sends `0x99`. Write it unconditionally on the way out.
        send("interruptVoice", glassesInterruptVoice())
        central.cancelPeripheralConnection(p)
    }

    func takePhoto() { send("takePhoto", glassesTakePhoto(forAi: false)) }
    func interruptVoice() { send("interruptVoice", glassesInterruptVoice()) }

    // MARK: - What the Wi-Fi screens need

    /// Forget the last announced SSID. Call this immediately BEFORE writing `0x39`/`0x67`:
    /// the glasses re-announce on every open, and joining a stale name is a twenty-second
    /// wait that ends in "unreachable".
    func clearWifiSSID() { wifiSSID = nil }

    /// Write a frame and wait for the ATT write to be acknowledged.
    ///
    /// `AA13` takes a Write REQUEST, so CoreBluetooth calls back once per write, in order —
    /// the continuations are a plain FIFO. Returns as soon as the radio has the frame; it
    /// says nothing about what the glasses will do with it, which is what the event stream
    /// is for. Never throws: a demo screen reports "not connected" in its own status line
    /// rather than unwinding a whole flow.
    func write(_ name: String, _ frame: Data) async {
        guard let p = peripheral, let c = writeCharacteristic else {
            note("not connected — dropped \(name)")
            return
        }
        let type: CBCharacteristicWriteType = gatt.writeWithResponse ? .withResponse : .withoutResponse
        append(.out, "\(name)  \(hex(frame))")
        guard type == .withResponse else {
            p.writeValue(frame, for: c, type: type)
            return
        }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            writeWaiters.append(continuation)
            p.writeValue(frame, for: c, type: type)
        }
    }

    /// Wait for the glasses to push their SSID (`0x25`), which arrives about
    /// `glassesTimingSsidArrivalMs()` after the open. The timeout is generous because the
    /// image processor has to power up first, and a slow one is not a broken one.
    func awaitSSID(timeout: TimeInterval = 15) async -> String? {
        if let ssid = wifiSSID, !ssid.isEmpty { return ssid }
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if Task.isCancelled { return nil }
            try? await Task.sleep(for: .milliseconds(200))
            if let ssid = wifiSSID, !ssid.isEmpty { return ssid }
        }
        return nil
    }

    /// Resumed in `didWriteValueFor`. FIFO, because `AA13` writes are acknowledged in the
    /// order they were issued.
    private var writeWaiters: [CheckedContinuation<Void, Never>] = []

    private func completeWrite() {
        guard !writeWaiters.isEmpty else { return }
        writeWaiters.removeFirst().resume()
    }

    private func failAllWrites() {
        let waiting = writeWaiters
        writeWaiters.removeAll()
        waiting.forEach { $0.resume() }
    }

    /// The connect interrogation. `getSwitchStates` (`0x48`) is the interesting one: the
    /// firmware answers with a TEN-FRAME BURST keyed by each setting's own setter opcode and
    /// never with a `0x48` frame, so a client that waits for one waits forever. The parser
    /// accumulates the burst; `status.settingsComplete` is it reporting that it has all ten.
    private func interrogate() {
        send("getVersions", glassesGetVersions())
        send("getProjectName", glassesGetProjectName())
        send("getBattery", glassesGetBattery())
        send("getCapabilities", glassesGetCapabilities())
        send("getSwitchStates", glassesGetSwitchStates())
    }

    private func send(_ name: String, _ frame: Data) {
        guard let p = peripheral, let c = writeCharacteristic else {
            note("not connected — dropped \(name)")
            return
        }
        // DEVICE-CONFIRMED: `AA13` takes an ATT Write REQUEST. `gatt.writeWithResponse` is
        // that fact as data; a Write Command gets no reply and no error either, which is the
        // single easiest way to waste an afternoon on this hardware.
        let type: CBCharacteristicWriteType = gatt.writeWithResponse ? .withResponse : .withoutResponse
        p.writeValue(frame, for: c, type: type)
        append(.out, "\(name)  \(hex(frame))")
    }

    // MARK: - Inbound

    private func ingest(_ data: Data, from uuid: CBUUID) {
        let parser = parsers[uuid] ?? {
            let p = GlassesParser()
            parsers[uuid] = p
            return p
        }()
        for event in parser.push(chunk: data) {
            apply(event)
            append(.incoming, describe(event))
            events.send(event)
        }
        if parser.switchStates().isComplete { status.settingsComplete = true }
    }

    /// Fold the events the header cares about. Everything else is log-only.
    private func apply(_ event: FfiGlassesEvent) {
        switch event {
        case let .battery(percent, charging, _):
            status.batteryPercent = percent
            status.charging = charging
        case let .versions(v):
            status.firmware = "bt \(v.btMajor).\(v.btMinor).\(v.btPatch) · isp \(v.ispMajor).\(v.ispMinor).\(v.ispPatch) · hw \(v.hardware)"
        case let .identity(project, customer):
            status.project = "\(project) / \(customer)"
        case let .wifiCredentials(ssid, _):
            // The passphrase half of this event is fixed and already in the crate
            // (`glassesWifiPassphrase()`); only the SSID changes per device.
            wifiSSID = ssid
        default:
            break
        }
    }

    // MARK: - Log

    private func note(_ text: String) { append(.note, text) }

    private func append(_ direction: LogLine.Direction, _ text: String) {
        log.append(LogLine(direction: direction, text: text))
        if log.count > 400 { log.removeFirst(log.count - 400) }
    }
}

// MARK: - CBCentralManagerDelegate

extension GlassesLink: CBCentralManagerDelegate {
    nonisolated func centralManagerDidUpdateState(_ central: CBCentralManager) {
        let state = central.state
        Task { @MainActor in
            self.note("bluetooth \(self.stateLabel(state))")
            if state != .poweredOn {
                self.isScanning = false
                self.status.phase = "bluetooth \(self.stateLabel(state))"
            }
        }
    }

    nonisolated func centralManager(
        _ central: CBCentralManager,
        didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any],
        rssi RSSI: NSNumber
    ) {
        Task { @MainActor in
            guard !self.found.contains(where: { $0.identifier == peripheral.identifier }) else { return }
            self.found.append(peripheral)
            self.note("found \(peripheral.name ?? "(unnamed)")  rssi \(RSSI)")
        }
    }

    nonisolated func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        Task { @MainActor in
            self.status.phase = "discovering"
            self.note("connected — discovering services")
            peripheral.discoverServices([self.serviceUUID])
        }
    }

    nonisolated func centralManager(
        _ central: CBCentralManager,
        didFailToConnect peripheral: CBPeripheral,
        error: Error?
    ) {
        let message = error?.localizedDescription ?? "unknown error"
        Task { @MainActor in
            self.status.phase = "idle"
            self.note("connect failed: \(message)")
        }
    }

    nonisolated func centralManager(
        _ central: CBCentralManager,
        didDisconnectPeripheral peripheral: CBPeripheral,
        error: Error?
    ) {
        let message = error?.localizedDescription
        Task { @MainActor in
            self.peripheral = nil
            self.writeCharacteristic = nil
            // A half-frame from the dead link would otherwise prefix the next one.
            self.parsers.removeAll()
            // Anything still awaiting a write ack will never get one now.
            self.failAllWrites()
            self.wifiSSID = nil
            self.status = LinkStatus()
            self.note("disconnected\(message.map { ": \($0)" } ?? "")")
        }
    }
}

// MARK: - CBPeripheralDelegate

extension GlassesLink: CBPeripheralDelegate {
    nonisolated func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        Task { @MainActor in
            guard let service = peripheral.services?.first(where: { $0.uuid == self.serviceUUID }) else {
                self.note("service \(self.gatt.service) not found")
                return
            }
            peripheral.discoverCharacteristics([self.writeUUID] + self.notifyUUIDs, for: service)
        }
    }

    nonisolated func peripheral(
        _ peripheral: CBPeripheral,
        didDiscoverCharacteristicsFor service: CBService,
        error: Error?
    ) {
        Task { @MainActor in
            let chars = service.characteristics ?? []
            // `glassesIsDrivable` is the crate's own answer to "can this peripheral be
            // driven?" — write plus at least the control notify.
            let drivable = glassesIsDrivable(characteristicUuids: chars.map { $0.uuid.uuidString })
            self.note("characteristics: \(chars.map { $0.uuid.uuidString }.joined(separator: ", ")) — drivable: \(drivable)")

            for c in chars {
                if c.uuid == self.writeUUID { self.writeCharacteristic = c }
                if self.notifyUUIDs.contains(c.uuid) { peripheral.setNotifyValue(true, for: c) }
            }
            self.status.phase = "connected"
            self.interrogate()
        }
    }

    nonisolated func peripheral(
        _ peripheral: CBPeripheral,
        didUpdateValueFor characteristic: CBCharacteristic,
        error: Error?
    ) {
        guard let data = characteristic.value else { return }
        let uuid = characteristic.uuid
        Task { @MainActor in self.ingest(data, from: uuid) }
    }

    nonisolated func peripheral(
        _ peripheral: CBPeripheral,
        didWriteValueFor characteristic: CBCharacteristic,
        error: Error?
    ) {
        let message = error?.localizedDescription
        Task { @MainActor in
            if let message { self.note("write failed: \(message)") }
            // Resume either way: a failed write must not strand a screen awaiting its ack.
            self.completeWrite()
        }
    }
}

// MARK: - Rendering

extension GlassesLink {
    fileprivate func stateLabel(_ state: CBManagerState) -> String {
        switch state {
        case .poweredOn: "on"
        case .poweredOff: "off"
        case .unauthorized: "unauthorized"
        case .unsupported: "unsupported"
        case .resetting: "resetting"
        default: "unknown"
        }
    }
}

func hex(_ data: Data) -> String {
    data.map { String(format: "%02x", $0) }.joined(separator: " ")
}

/// The event enum has no `CustomStringConvertible` across the FFI, so render the ones worth
/// reading and fall back to the derived description for the rest.
func describe(_ event: FfiGlassesEvent) -> String {
    switch event {
    case let .battery(percent, charging, source):
        return "battery \(percent)%\(charging ? " charging" : "") (\(source))"
    case let .versions(v):
        return "versions bt \(v.btMajor).\(v.btMinor).\(v.btPatch) isp \(v.ispMajor).\(v.ispMinor).\(v.ispPatch) hw \(v.hardware)"
    case let .identity(project, customer):
        return "identity project \(project) customer \(customer)"
    case let .mediaCount(count, source):
        return "media count \(count) (\(source))"
    case let .switch(report):
        return "setting \(report)"
    case let .gestureBinding(slot, action, raw):
        return "gesture \(slot) -> \(action.map { "\($0)" } ?? "unmapped") (raw \(raw))"
    case let .capabilities(raw, known):
        return "capabilities 0x\(String(raw, radix: 16))\(known ? " (known configuration)" : "")"
    case let .voiceFeatures(raw, allEnabled):
        return "voice features raw \(raw), all enabled \(allEnabled)"
    case let .volumes(system, media, call):
        return "volumes system \(system) media \(media) call \(call)"
    case let .wifiCredentials(ssid, passphrase):
        return "softAP up: ssid \(ssid) passphrase \(passphrase)"
    case let .wifiOpening(serves, raw):
        return "wifi opening for \(serves) (ack \(raw))"
    case let .actionSync(active, raw):
        return "action sync \(active) (raw 0x\(String(raw, radix: 16)))"
    case let .voice(event):
        return "voice \(event)"
    case let .ack(command, data):
        return "ack \(command)\(data.isEmpty ? "" : " \(hex(data))")"
    case let .malformed(cmd, data, reason):
        return "MALFORMED 0x\(String(cmd, radix: 16)) \(reason) \(hex(data))"
    case let .unrecognised(cmd, data):
        return "unrecognised 0x\(String(cmd, radix: 16)) \(hex(data))"
    default:
        return "\(event)"
    }
}
