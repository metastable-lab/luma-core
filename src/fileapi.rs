//! The Wi-Fi file API — URLs, the reply shapes, and the completeness rule.
//!
//! After `0x39` raises the SoftAP (§12) the glasses serve a small JSON-over-HTTP API on
//! `192.168.169.1`. This module owns everything about it that is arithmetic or a string:
//! the four paths, the percent-encoding, the two reply parsers, the KiB→bytes rule, and the
//! order a sync should walk. It owns no socket — [`crate::client::fileapi`] (feature
//! `wifi-client`) is the one that does.
//!
//! ```text
//! list       GET /app/getfilelist
//! thumbnail  GET /app/getthumbnail?file=EVENT/20260727223716.jpg
//! download   GET /EVENT/20260727223716.jpg
//! delete     GET /app/deletefile?file=EVENT/20260727223716.jpg
//! ```
//!
//! ## `size` is KiB, and this is the one fact that breaks clients
//!
//! The `size` field in the listing is **kibibytes**, not bytes, and it is a FLOOR. In a live
//! session a file listed as `size: 313` arrived as `Content-Length: 320900`, and
//! `320900 / 1024 = 313.37…`. Four files in that one listing, four exact matches of
//! `size == floor(bytes / 1024)`. A client that compares `Content-Length` to `size` for a
//! truncation check rejects every download it ever makes; a client that multiplies by 1024 and
//! demands equality rejects all but the 1-in-1024 that lands on a boundary.
//!
//! [`expected_byte_range`] and [`download_is_complete`] encode the rule once so no caller has
//! to re-derive it.
//!
//! ## The JSON reader
//!
//! Hand-rolled, in [`json`], because this crate's default build has zero dependencies and the
//! shape here is fixed and tiny. It is a real (if small) JSON parser rather than a substring
//! hunt: field order does not matter, unknown fields are ignored, and anything that is not
//! JSON comes back as a typed [`ParseError`] rather than a silent empty listing — which is what
//! a `find("\"name\"")` implementation does when the AP hands it an HTML error page.
//!
//! Depth is capped at [`json::MAX_DEPTH`]; the payload arrives from a device on an open AP and
//! a recursive-descent parser with no cap is a stack overflow waiting for one crafted reply.

use core::fmt;
use core::ops::RangeInclusive;

// ---------------------------------------------------------------------------------------
// The network, as data
// ---------------------------------------------------------------------------------------

/// The glasses' address on their own SoftAP. Fixed in firmware; verified in live sessions.
pub const HOST: &str = "192.168.169.1";

/// The address the phone/laptop is handed by the glasses' DHCP server.
///
/// Recorded because it is how a client can tell "joined the glasses' AP" from "still on the
/// house Wi-Fi" without a round trip.
pub const PHONE_ADDRESS: &str = "192.168.169.100";

/// The AP passphrase. Fixed in firmware, the same on every unit.
///
/// The same constant as [`crate::parser::WifiCredentials::PASSPHRASE`]; it is repeated here so a
/// Wi-Fi-only consumer of this module does not have to reach into the BLE half.
pub const PASSPHRASE: &str = crate::parser::WifiCredentials::PASSPHRASE;

/// `http://192.168.169.1` — no port, the server listens on 80.
pub const BASE_URL: &str = "http://192.168.169.1";

/// Path of the listing endpoint.
pub const LIST_PATH: &str = "/app/getfilelist";

/// Path of the thumbnail endpoint. Takes `?file=<FOLDER>/<name>`.
pub const THUMBNAIL_PATH: &str = "/app/getthumbnail";

/// Path of the delete endpoint. Takes `?file=<FOLDER>/<name>`.
pub const DELETE_PATH: &str = "/app/deletefile";

/// Build `http://<host>` for a host that is not the default (a proxy, a replay fixture).
pub fn base_url(host: &str) -> String {
    format!("http://{host}")
}

/// `http://192.168.169.1/app/getfilelist`.
pub fn list_url() -> String {
    format!("{BASE_URL}{LIST_PATH}")
}

/// Listing URL against an arbitrary host.
pub fn list_url_on(host: &str) -> String {
    format!("http://{host}{LIST_PATH}")
}

/// `http://192.168.169.1/app/getthumbnail?file=EVENT/2026…jpg`.
///
/// `name` is the listing's `name` field, which ALREADY carries the folder prefix — passing
/// `folder` and `name` separately and joining them is how a client ends up asking for
/// `EVENT/EVENT/2026….jpg`.
pub fn thumbnail_url(name: &str) -> String {
    thumbnail_url_on(HOST, name)
}

/// Thumbnail URL against an arbitrary host.
pub fn thumbnail_url_on(host: &str, name: &str) -> String {
    format!(
        "http://{host}{THUMBNAIL_PATH}?file={}",
        percent_encode_file_arg(name)
    )
}

/// `http://192.168.169.1/EVENT/2026…jpg` — the file itself.
pub fn download_url(name: &str) -> String {
    download_url_on(HOST, name)
}

/// Download URL against an arbitrary host.
pub fn download_url_on(host: &str, name: &str) -> String {
    format!("http://{host}{}", download_path(name))
}

/// `/EVENT/2026…jpg` — the download path alone, for a client that builds its own request line.
pub fn download_path(name: &str) -> String {
    let trimmed = name.trim_start_matches('/');
    format!("/{}", percent_encode_file_arg(trimmed))
}

/// `http://192.168.169.1/app/deletefile?file=EVENT/2026…jpg`.
pub fn delete_url(name: &str) -> String {
    delete_url_on(HOST, name)
}

/// Delete URL against an arbitrary host.
pub fn delete_url_on(host: &str, name: &str) -> String {
    format!(
        "http://{host}{DELETE_PATH}?file={}",
        percent_encode_file_arg(name)
    )
}

/// Percent-encode a `<FOLDER>/<name>` argument.
///
/// `/` is deliberately LEFT ALONE. The live sessions send `file=EVENT/20260727223716.jpg` with a
/// bare slash and the firmware answers; encoding it as `%2F` is defensible by RFC 3986 and is
/// not what the server was tested against. Everything outside the unreserved set
/// (`A-Z a-z 0-9 - . _ ~`) plus `/` is encoded.
pub fn percent_encode_file_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        let unreserved = c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'/');
        if unreserved {
            out.push(c as char);
        } else {
            out.push('%');
            out.push(HEX[(c >> 4) as usize] as char);
            out.push(HEX[(c & 0x0F) as usize] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

// ---------------------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------------------

/// The four folders the firmware serves. There are exactly these and the set is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Folder {
    /// Stills taken with `0x22`. Full-resolution JPEG, ~300–400 KB each.
    Event,
    /// Voice recordings started with `0x34`. AAC.
    Aac,
    /// Video clips from `0x23`/`0x24`, of the length set by `0x02`.
    Loop,
    /// Emergency clips. Present in every listing and empty in every one seen so far.
    Emr,
}

impl Folder {
    /// In the order the firmware lists them, which is also the order a sync should walk.
    pub const ALL: [Folder; 4] = [Folder::Event, Folder::Aac, Folder::Loop, Folder::Emr];

    /// The wire name — the `folder` field and the first path segment of every `name`.
    pub fn name(self) -> &'static str {
        match self {
            Folder::Event => "EVENT",
            Folder::Aac => "AAC",
            Folder::Loop => "LOOP",
            Folder::Emr => "EMR",
        }
    }

    /// Parse a wire folder name. Case-sensitive: the firmware only ever sends upper case.
    pub fn from_name(name: &str) -> Option<Folder> {
        Folder::ALL.into_iter().find(|f| f.name() == name)
    }

    /// What this folder holds. The `type` byte in the listing is per-file and has been `1` for
    /// every file seen; the folder is the reliable discriminator.
    pub fn holds(self) -> MediaKind {
        match self {
            Folder::Event => MediaKind::Photo,
            Folder::Aac => MediaKind::VoiceRecording,
            Folder::Loop => MediaKind::VideoClip,
            Folder::Emr => MediaKind::Emergency,
        }
    }

    /// The folder a `name` belongs to, read from its own prefix.
    pub fn of_name(name: &str) -> Option<Folder> {
        let head = name.trim_start_matches('/').split('/').next()?;
        Folder::from_name(head)
    }
}

/// What a file in a folder is. Derived from the folder, not from the `type` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MediaKind {
    Photo,
    VoiceRecording,
    VideoClip,
    Emergency,
}

// ---------------------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------------------

/// The `createtimestr` field, split into fields. Fourteen digits, `YYYYMMDDhhmmss`.
///
/// A plain struct and not a date type on purpose: this crate has no `chrono`, no time zone and
/// no clock. The glasses keep their own wall clock from `0x59` (§16), so the value is in
/// WHATEVER zone the phone was in when it last sent the time — converting it to an instant here
/// would be inventing an offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Timestamp {
    /// Parse `YYYYMMDDhhmmss`. Range-checks the fields; returns `None` on anything else.
    pub fn parse(s: &str) -> Option<Timestamp> {
        let b = s.as_bytes();
        if b.len() != 14 || !b.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let n = |from: usize, to: usize| -> u32 { s[from..to].parse().expect("digits") };
        let t = Timestamp {
            year: n(0, 4) as u16,
            month: n(4, 6) as u8,
            day: n(6, 8) as u8,
            hour: n(8, 10) as u8,
            minute: n(10, 12) as u8,
            second: n(12, 14) as u8,
        };
        if !(1..=12).contains(&t.month)
            || !(1..=31).contains(&t.day)
            || t.hour > 23
            || t.minute > 59
            || t.second > 60
        {
            return None;
        }
        Some(t)
    }

    /// Back to the fourteen-digit wire form.
    pub fn to_compact(self) -> String {
        format!(
            "{:04}{:02}{:02}{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl fmt::Display for Timestamp {
    /// `2026-07-27 22:37:16` — a rendering of the fields, not a localised date.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

// ---------------------------------------------------------------------------------------
// The listing
// ---------------------------------------------------------------------------------------

/// One row of one folder's `files` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The `name` field, verbatim: `EVENT/20260727223716.jpg`, folder prefix included. Every
    /// URL builder here takes exactly this.
    pub name: String,
    /// The folder the row was listed under.
    pub folder: Folder,
    /// The `size` field — **KiB, floored**. See the module docs; use [`expected_byte_range`].
    pub size_kib: u64,
    /// `createtimestr`, parsed. `None` when the field was present but not fourteen digits.
    pub created: Option<Timestamp>,
    /// What this is, from the folder.
    pub kind: MediaKind,
    /// The `type` field. `1` in every row seen; kept because nothing pins what else it can be.
    pub raw_type: i64,
}

impl FileEntry {
    /// The filename without its folder prefix — what to call it on local disk.
    pub fn basename(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }

    /// The byte count this file must land in to be whole. See [`expected_byte_range`].
    pub fn expected_byte_range(&self) -> RangeInclusive<u64> {
        expected_byte_range(self.size_kib)
    }

    /// Whether `bytes` is a complete download of this file.
    pub fn download_is_complete(&self, bytes: u64) -> bool {
        download_is_complete(self.size_kib, bytes)
    }
}

/// One folder's entry in the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderListing {
    pub folder: Folder,
    pub files: Vec<FileEntry>,
    /// The `count` field as the device sent it.
    ///
    /// It is NOT always `files.len()`: a live session listed `"count":2` beside a one-row
    /// `files` array. Kept raw rather than recomputed so the disagreement stays visible — see
    /// [`FolderListing::count_matches_rows`].
    pub count: u64,
}

impl FolderListing {
    /// Whether the device's own `count` agrees with the rows it sent.
    pub fn count_matches_rows(&self) -> bool {
        self.count == self.files.len() as u64
    }
}

/// A parsed `/app/getfilelist` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileList {
    /// The `result` field. `0` is success in every reply seen.
    pub result: i64,
    pub folders: Vec<FolderListing>,
}

impl FileList {
    /// Every file in every folder, in listing order.
    pub fn all_files(&self) -> impl Iterator<Item = &FileEntry> {
        self.folders.iter().flat_map(|f| f.files.iter())
    }

    /// The rows of one folder, or an empty slice if the device did not list it.
    pub fn files_in(&self, folder: Folder) -> &[FileEntry] {
        self.folders
            .iter()
            .find(|f| f.folder == folder)
            .map(|f| f.files.as_slice())
            .unwrap_or(&[])
    }

    /// How many rows the listing carries, across all folders.
    pub fn total_files(&self) -> usize {
        self.folders.iter().map(|f| f.files.len()).sum()
    }

    /// Sum of every row's `size_kib`. KiB, not bytes.
    pub fn total_size_kib(&self) -> u64 {
        self.all_files().map(|f| f.size_kib).sum()
    }

    /// Whether the glasses hold nothing at all.
    pub fn is_empty(&self) -> bool {
        self.total_files() == 0
    }

    /// Find one row by its `name`.
    pub fn find(&self, name: &str) -> Option<&FileEntry> {
        self.all_files().find(|f| f.name == name)
    }

    /// Parse a listing reply.
    pub fn parse(body: &str) -> Result<FileList, ParseError> {
        parse_file_list(body)
    }
}

/// A parsed `/app/deletefile` reply — `{"result":0,"info":"success."}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteReply {
    pub result: i64,
    /// The `info` field verbatim, trailing full stop and all.
    pub info: String,
}

impl DeleteReply {
    /// The exact `info` string a successful delete returns.
    pub const SUCCESS_INFO: &'static str = "success.";

    /// Whether the delete took. `result == 0` is the authority; the string is a courtesy.
    pub fn is_success(&self) -> bool {
        self.result == 0
    }

    /// Parse a delete reply.
    pub fn parse(body: &str) -> Result<DeleteReply, ParseError> {
        parse_delete_reply(body)
    }
}

// ---------------------------------------------------------------------------------------
// The completeness rule
// ---------------------------------------------------------------------------------------

/// Bytes per KiB. Named so the `1024` in the rule is not a bare literal.
pub const BYTES_PER_KIB: u64 = 1024;

/// The byte counts a file listed as `size_kib` may legitimately arrive as.
///
/// `size == floor(bytes / 1024)`, so `bytes ∈ [size*1024, size*1024 + 1023]`. Verified against
/// four downloads in one live session: 313→320,900, 338→346,328, 289→296,340, 270→277,308.
///
/// A file listed as `0` KiB is a real case (anything under 1 KiB) and its range is `0..=1023`.
pub fn expected_byte_range(size_kib: u64) -> RangeInclusive<u64> {
    let low = size_kib.saturating_mul(BYTES_PER_KIB);
    low..=low.saturating_add(BYTES_PER_KIB - 1)
}

/// Whether a download of `bytes` bytes satisfies a listing of `size_kib` KiB.
///
/// This is the ONLY truncation check that works here. `bytes == size_kib` fails always;
/// `bytes == size_kib * 1024` fails 1023 times in 1024.
pub fn download_is_complete(size_kib: u64, bytes: u64) -> bool {
    expected_byte_range(size_kib).contains(&bytes)
}

// ---------------------------------------------------------------------------------------
// The sync order
// ---------------------------------------------------------------------------------------

/// One action in a gallery sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStep {
    /// `GET /app/getfilelist`. Always first; everything after it is derived from the reply.
    List,
    /// `GET /app/getthumbnail?file=…`. Cheap (~8–12 KB) and it is what a picker renders.
    Thumbnail { name: String },
    /// `GET /<name>`. Check the result with [`download_is_complete`] against `size_kib`.
    Download { name: String, size_kib: u64 },
    /// `GET /app/deletefile?file=…`. Only ever emitted for a file this plan also downloads.
    Delete { name: String },
    /// Write `0x44 30 00` over BLE ([`crate::commands::file_download_complete`]) and drop the
    /// Wi-Fi network. Not an HTTP request — the step exists so a driver cannot forget it, which
    /// leaves the phone stranded on an AP with no route out.
    TearDown,
}

/// The recommended order for a full gallery sync.
///
/// List, then every thumbnail, then per file download-then-delete, then tear the AP down. That
/// is the order a live session walks, and the two halves matter for different reasons:
/// thumbnails first because a picker can paint while the big files are still moving, and delete
/// immediately after each download because the glasses' storage is small and a sync that
/// deletes only at the end loses everything it fetched if the AP drops halfway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    steps: Vec<SyncStep>,
}

impl SyncPlan {
    /// The plan for a listing already in hand. Starts at the thumbnails — [`SyncStep::List`] is
    /// the request that produced `list`.
    pub fn for_list(list: &FileList, delete_after: bool) -> SyncPlan {
        let mut steps = Vec::new();
        for f in list.all_files() {
            steps.push(SyncStep::Thumbnail {
                name: f.name.clone(),
            });
        }
        for f in list.all_files() {
            steps.push(SyncStep::Download {
                name: f.name.clone(),
                size_kib: f.size_kib,
            });
            if delete_after {
                steps.push(SyncStep::Delete {
                    name: f.name.clone(),
                });
            }
        }
        steps.push(SyncStep::TearDown);
        SyncPlan { steps }
    }

    /// The whole plan from cold, including the listing request itself.
    pub fn from_cold() -> SyncPlan {
        SyncPlan {
            steps: vec![SyncStep::List],
        }
    }

    /// The plan for a listing, with the listing request in front of it.
    pub fn full(list: &FileList, delete_after: bool) -> SyncPlan {
        let mut steps = vec![SyncStep::List];
        steps.extend(SyncPlan::for_list(list, delete_after).steps);
        SyncPlan { steps }
    }

    pub fn steps(&self) -> &[SyncStep] {
        &self.steps
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// How many bytes the downloads in this plan will move, at most. Upper bound of the KiB
    /// range, so a progress bar built on it never runs past 100 %.
    pub fn max_download_bytes(&self) -> u64 {
        self.steps
            .iter()
            .filter_map(|s| match s {
                SyncStep::Download { size_kib, .. } => Some(*expected_byte_range(*size_kib).end()),
                _ => None,
            })
            .sum()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, SyncStep> {
        self.steps.iter()
    }
}

impl IntoIterator for SyncPlan {
    type Item = SyncStep;
    type IntoIter = std::vec::IntoIter<SyncStep>;
    fn into_iter(self) -> Self::IntoIter {
        self.steps.into_iter()
    }
}

impl<'a> IntoIterator for &'a SyncPlan {
    type Item = &'a SyncStep;
    type IntoIter = core::slice::Iter<'a, SyncStep>;
    fn into_iter(self) -> Self::IntoIter {
        self.steps.iter()
    }
}

// ---------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------

/// Why a reply body is not the thing it claimed to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The body did not lex as JSON at all — an HTML error page, an empty body, a captive
    /// portal. Carries the byte offset the lexer gave up at.
    NotJson { at: usize },
    /// The body ended mid-value. Distinct from [`Self::NotJson`] because a caller streaming the
    /// response can WAIT on this one and must not on the other.
    Truncated,
    /// Bytes after the top-level value.
    TrailingBytes { at: usize },
    /// Nesting deeper than [`json::MAX_DEPTH`].
    TooDeep,
    /// A field that has to be there was not.
    MissingField { field: &'static str },
    /// A field was there and was the wrong JSON type.
    WrongType {
        field: &'static str,
        expected: &'static str,
    },
    /// A number field held something that is not an integer this side can hold.
    BadNumber { field: &'static str },
    /// A `folder` value outside `EVENT`/`AAC`/`LOOP`/`EMR`. Not tolerated: the folder set is
    /// fixed in firmware, so an unknown one means this is not a listing from these glasses.
    UnknownFolder { folder: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NotJson { at } => write!(f, "not JSON, at byte {at}"),
            ParseError::Truncated => write!(f, "JSON ended early"),
            ParseError::TrailingBytes { at } => write!(f, "trailing bytes at {at}"),
            ParseError::TooDeep => write!(f, "nested deeper than {}", json::MAX_DEPTH),
            ParseError::MissingField { field } => write!(f, "missing field `{field}`"),
            ParseError::WrongType { field, expected } => {
                write!(f, "field `{field}` is not {expected}")
            }
            ParseError::BadNumber { field } => write!(f, "field `{field}` is not an integer"),
            ParseError::UnknownFolder { folder } => write!(f, "unknown folder `{folder}`"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a `/app/getfilelist` reply body.
pub fn parse_file_list(body: &str) -> Result<FileList, ParseError> {
    let root = json::parse(body)?;
    let obj = root.object("body")?;
    let result = json::field(obj, "result")
        .ok_or(ParseError::MissingField { field: "result" })?
        .integer("result")?;

    // `info` is an ARRAY here and a STRING in the delete reply. Same field name, two shapes —
    // which is why the two replies get two parsers rather than one generic one.
    let folders_json = json::field(obj, "info")
        .ok_or(ParseError::MissingField { field: "info" })?
        .array("info")?;

    let mut folders = Vec::with_capacity(folders_json.len());
    for entry in folders_json {
        let e = entry.object("info[]")?;
        let folder_name = json::field(e, "folder")
            .ok_or(ParseError::MissingField { field: "folder" })?
            .string("folder")?;
        let folder = Folder::from_name(folder_name).ok_or_else(|| ParseError::UnknownFolder {
            folder: folder_name.to_string(),
        })?;
        let count = match json::field(e, "count") {
            Some(v) => v.integer("count")?.max(0) as u64,
            None => 0,
        };
        let files_json = json::field(e, "files")
            .ok_or(ParseError::MissingField { field: "files" })?
            .array("files")?;
        let mut files = Vec::with_capacity(files_json.len());
        for row in files_json {
            files.push(parse_file_entry(row, folder)?);
        }
        folders.push(FolderListing {
            folder,
            files,
            count,
        });
    }

    Ok(FileList { result, folders })
}

fn parse_file_entry(row: &json::Value, folder: Folder) -> Result<FileEntry, ParseError> {
    let o = row.object("files[]")?;
    let name = json::field(o, "name")
        .ok_or(ParseError::MissingField { field: "name" })?
        .string("name")?
        .to_string();
    let size_kib = json::field(o, "size")
        .ok_or(ParseError::MissingField { field: "size" })?
        .integer("size")?
        .max(0) as u64;
    // Rows carry their own folder prefix; trust that over the array they arrived in when the
    // two disagree, because every URL is built from `name`.
    let folder = Folder::of_name(&name).unwrap_or(folder);
    let created = json::field(o, "createtimestr")
        .and_then(|v| v.as_str())
        .and_then(Timestamp::parse);
    let raw_type = match json::field(o, "type") {
        Some(v) => v.integer("type")?,
        None => 0,
    };
    Ok(FileEntry {
        name,
        folder,
        size_kib,
        created,
        kind: folder.holds(),
        raw_type,
    })
}

/// Parse a `/app/deletefile` reply body — `{"result":0,"info":"success."}`.
pub fn parse_delete_reply(body: &str) -> Result<DeleteReply, ParseError> {
    let root = json::parse(body)?;
    let obj = root.object("body")?;
    let result = json::field(obj, "result")
        .ok_or(ParseError::MissingField { field: "result" })?
        .integer("result")?;
    let info = json::field(obj, "info")
        .ok_or(ParseError::MissingField { field: "info" })?
        .string("info")?
        .to_string();
    Ok(DeleteReply { result, info })
}

// ---------------------------------------------------------------------------------------
// A very small JSON reader
// ---------------------------------------------------------------------------------------

/// A minimal JSON reader, sized for the two replies above.
///
/// Public because the two parsers' errors reference it and because a consumer probing an
/// endpoint this crate does not model yet should not have to add a JSON dependency to look at
/// the reply. It is not a general-purpose library: numbers are kept as their source text and
/// converted on demand, which is exact for the integers this API sends and refuses the floats
/// it does not.
pub mod json {
    use super::ParseError;

    /// Deepest nesting accepted. The listing needs 3; anything near this is not a listing.
    pub const MAX_DEPTH: usize = 32;

    /// A JSON value. `Number` keeps its source text — see the module note.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(String),
        String(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    impl Value {
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::String(s) => Some(s),
                _ => None,
            }
        }

        pub fn as_i64(&self) -> Option<i64> {
            match self {
                Value::Number(n) => n.parse().ok(),
                _ => None,
            }
        }

        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(v) => Some(v),
                _ => None,
            }
        }

        pub fn as_object(&self) -> Option<&[(String, Value)]> {
            match self {
                Value::Object(v) => Some(v),
                _ => None,
            }
        }

        pub(super) fn object(&self, field: &'static str) -> Result<&[(String, Value)], ParseError> {
            self.as_object().ok_or(ParseError::WrongType {
                field,
                expected: "an object",
            })
        }

        pub(super) fn array(&self, field: &'static str) -> Result<&[Value], ParseError> {
            self.as_array().ok_or(ParseError::WrongType {
                field,
                expected: "an array",
            })
        }

        pub(super) fn string(&self, field: &'static str) -> Result<&str, ParseError> {
            self.as_str().ok_or(ParseError::WrongType {
                field,
                expected: "a string",
            })
        }

        pub(super) fn integer(&self, field: &'static str) -> Result<i64, ParseError> {
            match self {
                Value::Number(_) => self.as_i64().ok_or(ParseError::BadNumber { field }),
                _ => Err(ParseError::WrongType {
                    field,
                    expected: "a number",
                }),
            }
        }
    }

    /// Look one key up in an object's pairs. Linear, because these objects have five keys.
    ///
    /// FIELD ORDER DOES NOT MATTER and unknown keys are ignored: that is the whole reason the
    /// replies go through a parser rather than a substring hunt.
    pub fn field<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
        pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Parse one whole JSON document. Trailing whitespace is fine; trailing anything else is not.
    pub fn parse(src: &str) -> Result<Value, ParseError> {
        let mut p = Parser {
            b: src.as_bytes(),
            i: 0,
            depth: 0,
        };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.b.len() {
            return Err(ParseError::TrailingBytes { at: p.i });
        }
        Ok(v)
    }

    struct Parser<'a> {
        b: &'a [u8],
        i: usize,
        depth: usize,
    }

    impl<'a> Parser<'a> {
        fn ws(&mut self) {
            while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.i += 1;
            }
        }

        fn peek(&self) -> Result<u8, ParseError> {
            self.b.get(self.i).copied().ok_or(ParseError::Truncated)
        }

        fn eat(&mut self, c: u8) -> Result<(), ParseError> {
            if self.peek()? == c {
                self.i += 1;
                Ok(())
            } else {
                Err(ParseError::NotJson { at: self.i })
            }
        }

        fn lit(&mut self, word: &str, v: Value) -> Result<Value, ParseError> {
            if self.b[self.i..].starts_with(word.as_bytes()) {
                self.i += word.len();
                Ok(v)
            } else if word.as_bytes().starts_with(&self.b[self.i..]) {
                Err(ParseError::Truncated)
            } else {
                Err(ParseError::NotJson { at: self.i })
            }
        }

        fn value(&mut self) -> Result<Value, ParseError> {
            if self.depth >= MAX_DEPTH {
                return Err(ParseError::TooDeep);
            }
            match self.peek()? {
                b'{' => self.object(),
                b'[' => self.array(),
                b'"' => Ok(Value::String(self.string()?)),
                b't' => self.lit("true", Value::Bool(true)),
                b'f' => self.lit("false", Value::Bool(false)),
                b'n' => self.lit("null", Value::Null),
                c if c == b'-' || c.is_ascii_digit() => self.number(),
                _ => Err(ParseError::NotJson { at: self.i }),
            }
        }

        fn object(&mut self) -> Result<Value, ParseError> {
            self.eat(b'{')?;
            self.depth += 1;
            let mut pairs = Vec::new();
            self.ws();
            if self.peek()? == b'}' {
                self.i += 1;
                self.depth -= 1;
                return Ok(Value::Object(pairs));
            }
            loop {
                self.ws();
                let key = self.string()?;
                self.ws();
                self.eat(b':')?;
                self.ws();
                let v = self.value()?;
                pairs.push((key, v));
                self.ws();
                match self.peek()? {
                    b',' => self.i += 1,
                    b'}' => {
                        self.i += 1;
                        self.depth -= 1;
                        return Ok(Value::Object(pairs));
                    }
                    _ => return Err(ParseError::NotJson { at: self.i }),
                }
            }
        }

        fn array(&mut self) -> Result<Value, ParseError> {
            self.eat(b'[')?;
            self.depth += 1;
            let mut items = Vec::new();
            self.ws();
            if self.peek()? == b']' {
                self.i += 1;
                self.depth -= 1;
                return Ok(Value::Array(items));
            }
            loop {
                self.ws();
                items.push(self.value()?);
                self.ws();
                match self.peek()? {
                    b',' => self.i += 1,
                    b']' => {
                        self.i += 1;
                        self.depth -= 1;
                        return Ok(Value::Array(items));
                    }
                    _ => return Err(ParseError::NotJson { at: self.i }),
                }
            }
        }

        fn string(&mut self) -> Result<String, ParseError> {
            self.eat(b'"')?;
            let mut out = String::new();
            loop {
                let c = self.peek()?;
                self.i += 1;
                match c {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let e = self.peek()?;
                        self.i += 1;
                        match e {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'b' => out.push('\u{8}'),
                            b'f' => out.push('\u{c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'u' => {
                                let hex = self
                                    .b
                                    .get(self.i..self.i + 4)
                                    .ok_or(ParseError::Truncated)?;
                                let s = core::str::from_utf8(hex)
                                    .map_err(|_| ParseError::NotJson { at: self.i })?;
                                let n = u32::from_str_radix(s, 16)
                                    .map_err(|_| ParseError::NotJson { at: self.i })?;
                                self.i += 4;
                                // Lone surrogates become U+FFFD rather than failing the parse:
                                // the field this can appear in is a filename, and a filename
                                // with one bad code point still names a file.
                                out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                            }
                            _ => return Err(ParseError::NotJson { at: self.i }),
                        }
                    }
                    // Raw control characters are illegal in a JSON string.
                    0x00..=0x1F => return Err(ParseError::NotJson { at: self.i - 1 }),
                    _ => {
                        // Copy the whole UTF-8 sequence this byte starts.
                        let start = self.i - 1;
                        let len = utf8_len(c);
                        let end = start + len;
                        let raw = self.b.get(start..end).ok_or(ParseError::Truncated)?;
                        let s = core::str::from_utf8(raw)
                            .map_err(|_| ParseError::NotJson { at: start })?;
                        out.push_str(s);
                        self.i = end;
                    }
                }
            }
        }

        fn number(&mut self) -> Result<Value, ParseError> {
            let start = self.i;
            if self.peek()? == b'-' {
                self.i += 1;
            }
            let digits_from = self.i;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == digits_from {
                return Err(ParseError::NotJson { at: self.i });
            }
            // Fraction and exponent are accepted and kept as text; `as_i64` then refuses them,
            // which is the honest answer for a field this API only ever sends as an integer.
            if self.b.get(self.i) == Some(&b'.') {
                self.i += 1;
                while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
                self.i += 1;
                if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                    self.i += 1;
                }
                while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            let text = core::str::from_utf8(&self.b[start..self.i])
                .map_err(|_| ParseError::NotJson { at: start })?;
            Ok(Value::Number(text.to_string()))
        }
    }

    fn utf8_len(first: u8) -> usize {
        match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing from a live session: four photos in EVENT, three empty folders. Cut verbatim
    /// from the HTTP body of a `/app/getfilelist` reply.
    const LIVE_LISTING: &str = concat!(
        r#"{"result":0,"info":[{"folder":"EVENT","files":["#,
        r#"{"name":"EVENT/20260727223717716.jpg","size":313,"createtimestr":"20260727223716","type":1},"#,
        r#"{"name":"EVENT/20260727223720719.jpg","size":338,"createtimestr":"20260727223720","type":1},"#,
        r#"{"name":"EVENT/20260727223853716.jpg","size":289,"createtimestr":"20260727223852","type":1},"#,
        r#"{"name":"EVENT/20260727223856717.jpg","size":270,"createtimestr":"20260727223856","type":1}"#,
        r#"],"count":4},"#,
        r#"{"folder":"AAC","files":[],"count":0},"#,
        r#"{"folder":"LOOP","files":[],"count":0},"#,
        r#"{"folder":"EMR","files":[],"count":0}]}"#,
    );

    /// The sample printed in PROTOCOL.md §13 — one row, and `count` disagreeing with it.
    const SPEC_SAMPLE: &str = concat!(
        r#"{"result":0,"info":["#,
        r#"{"folder":"EVENT","files":[{"name":"EVENT/20260727225147720.jpg","size":378,"#,
        r#""createtimestr":"20260727225146","type":1}],"count":2},"#,
        r#"{"folder":"AAC","files":[],"count":0},"#,
        r#"{"folder":"LOOP","files":[],"count":0},"#,
        r#"{"folder":"EMR","files":[],"count":0}]}"#,
    );

    #[test]
    fn the_live_listing_parses_into_four_photos_and_three_empty_folders() {
        let list = FileList::parse(LIVE_LISTING).expect("a listing");
        assert_eq!(list.result, 0);
        assert_eq!(list.folders.len(), 4);
        assert_eq!(list.total_files(), 4);
        assert_eq!(list.files_in(Folder::Aac), &[]);
        assert_eq!(list.files_in(Folder::Loop), &[]);
        assert_eq!(list.files_in(Folder::Emr), &[]);

        let first = &list.files_in(Folder::Event)[0];
        assert_eq!(first.name, "EVENT/20260727223717716.jpg");
        assert_eq!(first.basename(), "20260727223717716.jpg");
        assert_eq!(first.size_kib, 313);
        assert_eq!(first.kind, MediaKind::Photo);
        assert_eq!(first.raw_type, 1);
        assert_eq!(
            first.created,
            Some(Timestamp {
                year: 2026,
                month: 7,
                day: 27,
                hour: 22,
                minute: 37,
                second: 16
            })
        );
        assert_eq!(first.created.unwrap().to_string(), "2026-07-27 22:37:16");
    }

    /// The four `Content-Length`s the four rows above actually downloaded as, in the same
    /// session. This is the pair of numbers the KiB rule exists for.
    #[test]
    fn every_download_in_the_live_session_satisfies_the_kib_rule() {
        for (size_kib, bytes) in [
            (313u64, 320_900u64),
            (338, 346_328),
            (289, 296_340),
            (270, 277_308),
        ] {
            assert!(
                download_is_complete(size_kib, bytes),
                "{size_kib} KiB should accept {bytes} bytes"
            );
            assert_eq!(size_kib, bytes / 1024, "the listing is floor(bytes/1024)");
            assert!(!download_is_complete(size_kib, bytes - 1024));
            assert!(!download_is_complete(size_kib, bytes + 1024));
        }
    }

    #[test]
    fn a_byte_exact_check_against_size_would_reject_every_download() {
        // The two wrong rules, stated so a future edit cannot quietly reintroduce one.
        assert!(!download_is_complete(313, 313));
        assert!(!download_is_complete(313, 313 * 1024 + 1024));
        assert_eq!(expected_byte_range(313), 320_512..=321_535);
        assert!(expected_byte_range(313).contains(&320_900));
    }

    #[test]
    fn a_file_under_one_kib_lists_as_zero_and_still_has_a_range() {
        assert_eq!(expected_byte_range(0), 0..=1023);
        assert!(download_is_complete(0, 1));
        assert!(download_is_complete(0, 1023));
        assert!(!download_is_complete(0, 1024));
    }

    #[test]
    fn the_spec_sample_parses_and_its_count_disagrees_with_its_rows() {
        let list = FileList::parse(SPEC_SAMPLE).expect("a listing");
        assert_eq!(list.total_files(), 1);
        let event = &list.folders[0];
        assert_eq!(event.count, 2);
        assert_eq!(event.files.len(), 1);
        assert!(
            !event.count_matches_rows(),
            "the device's own count is kept raw so the disagreement is visible"
        );
    }

    #[test]
    fn field_order_does_not_matter_and_unknown_fields_are_ignored() {
        let shuffled = concat!(
            r#"{"info":[{"count":1,"unknown":{"a":[1,2,3]},"files":["#,
            r#"{"type":1,"size":378,"unseen":null,"createtimestr":"20260727225146","#,
            r#""name":"EVENT/20260727225147720.jpg"}],"folder":"EVENT"}],"result":0}"#
        );
        let list = FileList::parse(shuffled).expect("a listing");
        assert_eq!(list.total_files(), 1);
        assert_eq!(list.all_files().next().unwrap().size_kib, 378);
    }

    #[test]
    fn an_entirely_empty_gallery_parses_as_four_empty_folders() {
        let empty = concat!(
            r#"{"result":0,"info":[{"folder":"EVENT","files":[],"count":0},"#,
            r#"{"folder":"AAC","files":[],"count":0},"#,
            r#"{"folder":"LOOP","files":[],"count":0},"#,
            r#"{"folder":"EMR","files":[],"count":0}]}"#
        );
        let list = FileList::parse(empty).expect("a listing");
        assert!(list.is_empty());
        assert_eq!(list.folders.len(), 4);
        assert_eq!(list.total_size_kib(), 0);
        assert_eq!(
            SyncPlan::for_list(&list, true).steps(),
            &[SyncStep::TearDown]
        );
    }

    #[test]
    fn a_hundred_percent_ascii_name_survives_the_url_builders() {
        let name = "EVENT/20260727223717716.jpg";
        assert_eq!(
            thumbnail_url(name),
            "http://192.168.169.1/app/getthumbnail?file=EVENT/20260727223717716.jpg"
        );
        assert_eq!(
            download_url(name),
            "http://192.168.169.1/EVENT/20260727223717716.jpg"
        );
        assert_eq!(
            delete_url(name),
            "http://192.168.169.1/app/deletefile?file=EVENT/20260727223717716.jpg"
        );
        assert_eq!(list_url(), "http://192.168.169.1/app/getfilelist");
    }

    #[test]
    fn anything_outside_the_unreserved_set_is_percent_encoded_but_the_slash_is_not() {
        assert_eq!(percent_encode_file_arg("EVENT/a b.jpg"), "EVENT/a%20b.jpg");
        assert_eq!(percent_encode_file_arg("EVENT/a&b=c"), "EVENT/a%26b%3Dc");
        assert_eq!(percent_encode_file_arg("EVENT/../etc"), "EVENT/../etc");
        assert_eq!(percent_encode_file_arg("A-Z_a.z~0"), "A-Z_a.z~0");
        // A leading slash in a name would otherwise produce `//EVENT/…`.
        assert_eq!(download_path("/EVENT/x.jpg"), "/EVENT/x.jpg");
    }

    #[test]
    fn an_unknown_folder_is_rejected_rather_than_silently_dropped() {
        let body = r#"{"result":0,"info":[{"folder":"MOVIES","files":[],"count":0}]}"#;
        assert_eq!(
            FileList::parse(body),
            Err(ParseError::UnknownFolder {
                folder: "MOVIES".to_string()
            })
        );
    }

    #[test]
    fn truncated_json_is_a_different_error_from_garbage() {
        let cut = &LIVE_LISTING[..LIVE_LISTING.len() - 20];
        assert_eq!(FileList::parse(cut), Err(ParseError::Truncated));
        assert_eq!(FileList::parse(""), Err(ParseError::Truncated));

        // A captive portal or an error page, which is what a wrong network actually returns.
        assert!(matches!(
            FileList::parse("<html><body>404</body></html>"),
            Err(ParseError::NotJson { at: 0 })
        ));
        assert!(matches!(
            FileList::parse(r#"{"result":0} trailing"#),
            Err(ParseError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn a_listing_missing_a_required_field_names_the_field() {
        assert_eq!(
            FileList::parse(r#"{"info":[]}"#),
            Err(ParseError::MissingField { field: "result" })
        );
        assert_eq!(
            FileList::parse(r#"{"result":0}"#),
            Err(ParseError::MissingField { field: "info" })
        );
        assert_eq!(
            FileList::parse(r#"{"result":0,"info":[{"folder":"EVENT","count":0}]}"#),
            Err(ParseError::MissingField { field: "files" })
        );
        assert_eq!(
            FileList::parse(r#"{"result":"0","info":[]}"#),
            Err(ParseError::WrongType {
                field: "result",
                expected: "a number"
            })
        );
    }

    #[test]
    fn a_bad_createtimestr_leaves_the_row_usable_with_no_timestamp() {
        let body = concat!(
            r#"{"result":0,"info":[{"folder":"EVENT","count":1,"files":["#,
            r#"{"name":"EVENT/x.jpg","size":1,"createtimestr":"nonsense","type":1}]}]}"#
        );
        let list = FileList::parse(body).expect("a listing");
        let row = list.all_files().next().unwrap();
        assert_eq!(
            row.created, None,
            "unparseable, not fatal — the file is still there"
        );
        assert_eq!(row.name, "EVENT/x.jpg");
    }

    #[test]
    fn nesting_past_the_cap_is_refused_rather_than_overflowing_the_stack() {
        let deep = "[".repeat(json::MAX_DEPTH + 2);
        assert_eq!(json::parse(&deep), Err(ParseError::TooDeep));
    }

    /// The delete reply, cut verbatim from a live session.
    #[test]
    fn the_delete_reply_parses_and_result_zero_is_the_authority() {
        let reply = DeleteReply::parse(r#"{"result":0,"info":"success."}"#).expect("a reply");
        assert_eq!(reply.result, 0);
        assert_eq!(reply.info, DeleteReply::SUCCESS_INFO);
        assert!(reply.is_success());

        let failed = DeleteReply::parse(r#"{"result":-1,"info":"no such file"}"#).unwrap();
        assert!(!failed.is_success());

        // `info` is an array in the listing and a string here — one field name, two shapes.
        assert_eq!(
            DeleteReply::parse(r#"{"result":0,"info":[]}"#),
            Err(ParseError::WrongType {
                field: "info",
                expected: "a string"
            })
        );
    }

    #[test]
    fn a_folder_knows_what_it_holds_and_a_name_knows_its_folder() {
        assert_eq!(Folder::Event.holds(), MediaKind::Photo);
        assert_eq!(Folder::Aac.holds(), MediaKind::VoiceRecording);
        assert_eq!(Folder::Loop.holds(), MediaKind::VideoClip);
        assert_eq!(Folder::Emr.holds(), MediaKind::Emergency);
        assert_eq!(Folder::of_name("AAC/2026.aac"), Some(Folder::Aac));
        assert_eq!(Folder::of_name("nope/x"), None);
        for f in Folder::ALL {
            assert_eq!(Folder::from_name(f.name()), Some(f));
        }
        assert_eq!(Folder::from_name("event"), None, "the wire is upper case");
    }

    #[test]
    fn the_sync_plan_lists_then_thumbnails_then_downloads_each_file_before_deleting_it() {
        let list = FileList::parse(SPEC_SAMPLE).unwrap();
        let plan = SyncPlan::full(&list, true);
        assert_eq!(
            plan.steps(),
            &[
                SyncStep::List,
                SyncStep::Thumbnail {
                    name: "EVENT/20260727225147720.jpg".into()
                },
                SyncStep::Download {
                    name: "EVENT/20260727225147720.jpg".into(),
                    size_kib: 378
                },
                SyncStep::Delete {
                    name: "EVENT/20260727225147720.jpg".into()
                },
                SyncStep::TearDown,
            ]
        );
        assert_eq!(plan.max_download_bytes(), 378 * 1024 + 1023);
    }

    #[test]
    fn a_read_only_sync_plan_never_emits_a_delete() {
        let list = FileList::parse(LIVE_LISTING).unwrap();
        let plan = SyncPlan::for_list(&list, false);
        assert!(!plan.iter().any(|s| matches!(s, SyncStep::Delete { .. })));
        assert_eq!(plan.len(), 4 + 4 + 1);
        assert_eq!(plan.steps().last(), Some(&SyncStep::TearDown));
    }

    #[test]
    fn the_ap_facts_are_the_ones_in_the_protocol_reference() {
        assert_eq!(HOST, "192.168.169.1");
        assert_eq!(PHONE_ADDRESS, "192.168.169.100");
        assert_eq!(PASSPHRASE, "12345678");
        assert_eq!(BASE_URL, base_url(HOST));
    }
}
