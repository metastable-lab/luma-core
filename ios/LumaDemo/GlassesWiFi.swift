//
//  GlassesWiFi.swift
//  One deliberate, self-reverting hop onto the glasses' own access point.
//
//  The glasses raise a SoftAP on demand (PROTOCOL.md §12): a BLE write opens it, the
//  glasses push their SSID back over BLE, and everything media-shaped — the file API
//  (§13) and the live stream (§14) — travels over that network instead of over BLE.
//
//  This file owns `NEHotspotConfiguration`, `NWConnection` and `URLSession`. It owns no
//  protocol knowledge at all: the passphrase, the host address and the join timeout all
//  come out of `LumaCore`:
//
//      glassesWifiPassphrase()          the fixed WPA key
//      glassesWifiHost()                192.168.169.1
//      glassesTimingWifiJoinTimeoutMs() how long a join is allowed to take
//      glassesTimingSsidSettleMs()      how long to wait AFTER the SSID before joining
//
//  iOS has a single Wi-Fi radio and no Wi-Fi Direct, so joining the glasses means
//  leaving the user's network for the duration. `joinOnce` keeps the network out of the
//  saved list, and every caller removes the configuration on the way out, so the phone
//  snaps back to normal Wi-Fi as soon as the screen is dismissed.
//

import Foundation
import Network
import NetworkExtension
import LumaCore

enum GlassesWiFi {

    /// What can go wrong between "the glasses said they have a network" and "we can talk
    /// to them on it". Each case is worth showing verbatim on a demo screen.
    enum WiFiError: LocalizedError {
        case noSSID
        case joinFailed(String)
        case unreachable(host: String, port: UInt16, seconds: Int)

        var errorDescription: String? {
            switch self {
            case .noSSID:
                return "The glasses never pushed an SSID. Their access point did not come up."
            case let .joinFailed(message):
                return "Could not join the glasses Wi-Fi: \(message)"
            case let .unreachable(host, port, seconds):
                return "Joined, but \(host):\(port) did not answer within \(seconds)s. "
                    + "Open Settings ▸ Wi-Fi and join the glasses network by hand, then try again."
            }
        }
    }

    // MARK: - Timing, from the crate

    /// The glasses announce the SSID over BLE *before* the radio is actually associable —
    /// the image processor is still bringing it up. Joining immediately is what produces
    /// iOS's "added to known networks, but association failed" a few milliseconds later.
    /// The number is `timing::SSID_SETTLE` in the crate, not a guess made here.
    static var ssidSettle: Duration { .milliseconds(Int(glassesTimingSsidSettleMs())) }

    /// The whole budget for "iOS accepted the config" plus "the server answers". iOS
    /// routinely takes twenty seconds to join a hotspot with no internet on it.
    static var joinTimeout: TimeInterval { Double(glassesTimingWifiJoinTimeoutMs()) / 1000 }

    // MARK: - URLSession

    /// An ephemeral session pinned to the access point: no cellular fallback (the host is
    /// link-local, so a cellular attempt can only fail slowly), no caching (a listing that
    /// came back 200 once must not be replayed after a delete), and short timeouts so a
    /// failed join fails fast instead of hanging the screen.
    static func makeSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.waitsForConnectivity = false
        configuration.allowsCellularAccess = false
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 120
        configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
        return URLSession(configuration: configuration)
    }

    // MARK: - Join and leave

    /// Apply one hotspot configuration for `ssid`.
    ///
    /// `joinOnce` means iOS drops the network the moment the app stops using it and never
    /// adds it to the user's saved networks. `hidden` is set because a BLE-announced
    /// SoftAP frequently does not beacon its SSID, and iOS fails a passive scan for one it
    /// cannot see — instantly, and with a success callback, which is the confusing part.
    ///
    /// The callback returning without an error only means "added to known networks". It is
    /// *not* proof of association; that is what `waitForHost` is for.
    static func join(ssid: String) async throws {
        let ssid = ssid.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !ssid.isEmpty else { throw WiFiError.noSSID }

        // The passphrase is not on the wire — the SSID frame carries only the name — so it
        // lives in the crate beside the frame that needs it rather than as a literal here.
        let configuration = NEHotspotConfiguration(
            ssid: ssid,
            passphrase: glassesWifiPassphrase(),
            isWEP: false
        )
        configuration.joinOnce = true
        configuration.hidden = true

        do {
            try await apply(configuration)
        } catch {
            throw WiFiError.joinFailed(error.localizedDescription)
        }
    }

    /// Remove the configuration so iOS returns the phone to its normal network. Safe to
    /// call twice, and safe to call for an SSID that was never joined.
    static func leave(ssid: String) {
        let ssid = ssid.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !ssid.isEmpty else { return }
        NEHotspotConfigurationManager.shared.removeConfiguration(forSSID: ssid)
    }

    private static func apply(_ configuration: NEHotspotConfiguration) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            NEHotspotConfigurationManager.shared.apply(configuration) { error in
                // Already on the network is exactly what we wanted, not a failure.
                if let error = error as NSError?,
                   error.domain == NEHotspotConfigurationErrorDomain,
                   error.code == NEHotspotConfigurationError.alreadyAssociated.rawValue {
                    continuation.resume()
                    return
                }
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume()
                }
            }
        }
    }

    // MARK: - Reachability

    /// Poll `glassesWifiHost()` on `port` until it accepts a TCP connection, or the crate's
    /// join timeout elapses.
    ///
    /// A TCP connect rather than an HTTP GET on purpose. Whichever way the phone ends up on
    /// the access point — the programmatic join, or the user picking the network in
    /// Settings — a socket that opens on the glasses' fixed address is the only honest
    /// signal that we are actually there. `onProgress` is called each second so a demo
    /// screen can count the wait out loud instead of showing a still spinner.
    @discardableResult
    static func waitForHost(
        port: UInt16,
        onProgress: @MainActor (Int) -> Void = { _ in }
    ) async throws -> String {
        let host = glassesWifiHost()
        let deadline = Date().addingTimeInterval(joinTimeout)
        var elapsed = 0

        while Date() < deadline {
            try Task.checkCancellation()
            if await probe(host: host, port: port, timeout: 2) { return host }
            elapsed += 1
            await onProgress(elapsed)
            try? await Task.sleep(for: .seconds(1))
        }
        throw WiFiError.unreachable(host: host, port: port, seconds: Int(joinTimeout))
    }

    /// One TCP connect attempt, with its own timeout. `NWConnection` reports both success
    /// and failure asynchronously and can report neither, so the continuation is guarded by
    /// a resume-once box and a hard deadline.
    private static func probe(host: String, port: UInt16, timeout: TimeInterval) async -> Bool {
        await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            let once = ResumeOnce()
            let endpoint = NWEndpoint.Host(host)
            guard let nwPort = NWEndpoint.Port(rawValue: port) else {
                continuation.resume(returning: false)
                return
            }
            let connection = NWConnection(host: endpoint, port: nwPort, using: .tcp)
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    once.run { connection.cancel(); continuation.resume(returning: true) }
                case .failed, .cancelled:
                    once.run { continuation.resume(returning: false) }
                default:
                    break
                }
            }
            connection.start(queue: .global(qos: .userInitiated))
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) {
                once.run { connection.cancel(); continuation.resume(returning: false) }
            }
        }
    }
}

/// A `CheckedContinuation` resumed twice is a crash, and `NWConnection` will happily report
/// `.cancelled` right after `.ready`. One tiny lock keeps every probe honest.
final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func run(_ body: () -> Void) {
        lock.lock()
        if done {
            lock.unlock()
            return
        }
        done = true
        lock.unlock()
        body()
    }
}
