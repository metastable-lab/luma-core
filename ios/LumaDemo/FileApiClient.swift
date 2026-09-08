//
//  FileApiClient.swift
//  The glasses' file API, over the Wi-Fi access point they raise for it.
//
//  Four GETs on `http://192.168.169.1` and nothing else (PROTOCOL.md §13). Every URL and
//  every byte of parsing comes from `LumaCore`; this file owns a `URLSession` and the app's
//  Documents directory, and that is the whole of it:
//
//      glassesWifiListUrl()                     GET the JSON listing
//      glassesWifiThumbnailUrl(name:)           GET a thumbnail JPEG
//      glassesWifiDownloadUrl(name:)            GET the file itself
//      glassesWifiDeleteUrl(name:)              GET to delete it
//      glassesWifiParseFileList(json:)          listing JSON  → typed folders and entries
//      glassesWifiParseDeleteReply(json:)       delete JSON   → success / failure
//      glassesWifiDownloadIsComplete(sizeKib:bytes:)  the KiB completeness rule
//      glassesWifiFolderName(folder:)           EVENT / AAC / LOOP / EMR
//      glassesWifiFolderHolds(folder:)          what kind of media that folder carries
//
//  The completeness rule is the one that bites. The listing's `size` is in **KiB, floored**,
//  not bytes: a file listed as `378` downloads as 387,972 bytes. Comparing the byte count to
//  `size` directly rejects every download ever made. `glassesWifiDownloadIsComplete` is that
//  rule, tested in the crate, so no shell has to rediscover it.
//

import Foundation
import LumaCore

/// A file the glasses hold, as the crate parsed it, plus whatever this app has since done
/// with it. `FfiGlassesFileEntry` is the protocol half and is never edited here.
struct GalleryItem: Identifiable, Equatable {
    let entry: FfiGlassesFileEntry

    /// `EVENT/20260727225147720.jpg` — unique on the device, and the key every URL builder
    /// in the crate takes.
    var id: String { entry.name }

    var basename: String { entry.basename }
    var folder: FfiGlassesFolder { entry.folder }
    var kind: FfiGlassesMediaKind { entry.kind }

    /// The listing's KiB figure, rendered for a caption.
    var sizeText: String {
        let kib = entry.sizeKib
        return kib >= 1024
            ? String(format: "%.1f MB", Double(kib) / 1024)
            : "\(kib) KB"
    }

    /// The glasses timestamp their files in the filename and repeat it in `createtimestr`;
    /// the crate has already split it into fields.
    var createdText: String {
        guard let t = entry.created else { return "—" }
        return String(
            format: "%04d-%02d-%02d %02d:%02d:%02d",
            Int(t.year), Int(t.month), Int(t.day), Int(t.hour), Int(t.minute), Int(t.second)
        )
    }

    /// Only the photo folders have thumbnails worth asking for; a voice note has none and
    /// the request just 404s slowly.
    var hasThumbnail: Bool {
        switch kind {
        case .photo, .videoClip, .emergency: return true
        case .voiceRecording: return false
        }
    }

    var systemImage: String {
        switch kind {
        case .photo: return "photo"
        case .voiceRecording: return "waveform"
        case .videoClip: return "video"
        case .emergency: return "exclamationmark.triangle"
        }
    }
}

/// One folder's worth of entries, in the order the crate reported them.
struct GallerySection: Identifiable, Equatable {
    let folder: FfiGlassesFolder
    let items: [GalleryItem]
    /// The device's own `count`. It does NOT always match `items.count` — a live listing
    /// reported 2 beside a one-row array — so both are shown rather than reconciled.
    let deviceCount: UInt64

    var id: String { glassesWifiFolderName(folder: folder) }
    var title: String { glassesWifiFolderName(folder: folder) }

    var subtitle: String {
        let holds: String
        switch glassesWifiFolderHolds(folder: folder) {
        case .photo: holds = "photos"
        case .voiceRecording: holds = "voice recordings"
        case .videoClip: holds = "video clips"
        case .emergency: holds = "emergency clips"
        }
        return items.count == Int(deviceCount)
            ? "\(items.count) \(holds)"
            : "\(items.count) \(holds) (device says \(deviceCount))"
    }
}

/// A thin HTTP client for the four endpoints. Stateless apart from the session.
struct FileApiClient {

    enum ApiError: LocalizedError {
        case badStatus(Int)
        case notText
        case parse(FfiGlassesWifiParseError)
        case deleteRefused(result: Int64, info: String)
        case incompleteDownload(name: String, expectedKib: UInt64, got: Int)

        var errorDescription: String? {
            switch self {
            case let .badStatus(code):
                return "The glasses answered HTTP \(code)."
            case .notText:
                return "The glasses answered with something that is not text."
            case let .parse(reason):
                return "Could not read the reply: \(FileApiClient.describe(reason))"
            case let .deleteRefused(result, info):
                return "The glasses refused the delete (result \(result): \(info))."
            case let .incompleteDownload(name, kib, got):
                return "\(name) arrived truncated — \(got) bytes for a file listed at \(kib) KB."
            }
        }
    }

    let session: URLSession

    // MARK: - The four calls

    /// `GET /app/getfilelist`, folded into per-folder sections.
    func list() async throws -> (sections: [GallerySection], totalFiles: UInt32, totalKib: UInt64) {
        let json = try await getText(glassesWifiListUrl())
        switch glassesWifiParseFileList(json: json) {
        case let .err(reason):
            throw ApiError.parse(reason)
        case let .ok(list):
            let sections = list.folders.map { listing in
                GallerySection(
                    folder: listing.folder,
                    items: listing.files.map(GalleryItem.init(entry:)),
                    deviceCount: listing.count
                )
            }
            return (sections, list.totalFiles, list.totalSizeKib)
        }
    }

    /// `GET /app/getthumbnail?file=<FOLDER>/<name>`. Best effort — a missing thumbnail is
    /// a blank tile, never an error the screen has to report.
    func thumbnail(for item: GalleryItem) async -> Data? {
        guard item.hasThumbnail else { return nil }
        return try? await getData(glassesWifiThumbnailUrl(name: item.entry.name))
    }

    /// `GET /<FOLDER>/<name>`, verified against the listing's KiB figure and written into
    /// the app's Documents directory.
    ///
    /// `glassesWifiDownloadIsComplete` is the crate's KiB rule; the byte count on its own
    /// proves nothing, because the listing floors to whole KiB. A file whose byte count
    /// falls outside `[minBytes, maxBytes]` came back short and is thrown away rather than
    /// saved as a half-picture.
    func download(_ item: GalleryItem) async throws -> URL {
        let data = try await getData(glassesWifiDownloadUrl(name: item.entry.name))
        guard glassesWifiDownloadIsComplete(sizeKib: item.entry.sizeKib, bytes: UInt64(data.count)) else {
            throw ApiError.incompleteDownload(
                name: item.basename,
                expectedKib: item.entry.sizeKib,
                got: data.count
            )
        }
        let destination = Self.documentsDirectory().appendingPathComponent(item.basename)
        try data.write(to: destination, options: .atomic)
        return destination
    }

    /// `GET /app/deletefile?file=<FOLDER>/<name>`. The reply is `{"result":0,"info":"success."}`;
    /// the crate reads `result`, which is the authority — the string is a courtesy.
    func delete(_ item: GalleryItem) async throws {
        let json = try await getText(glassesWifiDeleteUrl(name: item.entry.name))
        switch glassesWifiParseDeleteReply(json: json) {
        case let .err(reason):
            throw ApiError.parse(reason)
        case let .ok(reply):
            guard reply.success else {
                throw ApiError.deleteRefused(result: reply.result, info: reply.info)
            }
        }
    }

    // MARK: - Transport

    private func getData(_ urlString: String) async throws -> Data {
        guard let url = URL(string: urlString) else { throw ApiError.notText }
        var request = URLRequest(url: url)
        request.cachePolicy = .reloadIgnoringLocalAndRemoteCacheData
        let (data, response) = try await session.data(for: request)
        if let http = response as? HTTPURLResponse, http.statusCode != 200 {
            throw ApiError.badStatus(http.statusCode)
        }
        return data
    }

    private func getText(_ urlString: String) async throws -> String {
        let data = try await getData(urlString)
        // The parsers take a `String`; the server sends ASCII JSON, so a lossy UTF-8
        // decode is the right fallback for a stray byte rather than a hard failure.
        guard let text = String(data: data, encoding: .utf8)
                ?? String(data: data, encoding: .isoLatin1) else {
            throw ApiError.notText
        }
        return text
    }

    // MARK: - Rendering

    /// The crate's parse errors say precisely what was wrong with the body. Worth putting
    /// on screen verbatim: "not JSON at byte 0" is a captive portal or the phone still
    /// being on the wrong network, and that is exactly the failure a first run hits.
    static func describe(_ reason: FfiGlassesWifiParseError) -> String {
        switch reason {
        case let .notJson(at):
            return "not JSON at byte \(at) — probably an error page, or the phone is not on the glasses network"
        case .truncated:
            return "the body ended mid-value"
        case let .trailingBytes(at):
            return "extra bytes after the JSON at \(at)"
        case .tooDeep:
            return "nested too deeply to be a listing"
        case let .missingField(field):
            return "missing field \(field)"
        case let .wrongType(field, expected):
            return "field \(field) is not \(expected)"
        case let .badNumber(field):
            return "field \(field) is not a number"
        case let .unknownFolder(folder):
            return "unknown folder \(folder) — not a listing from these glasses"
        }
    }

    static func documentsDirectory() -> URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }
}
