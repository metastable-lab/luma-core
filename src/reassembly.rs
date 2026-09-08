//! The `52 58` file stream on `AA15`: a second envelope, and the chunked-transfer accumulator
//! that rides it.
//!
//! Port of the reference iOS client's file-frame reassembler (205 lines of Swift) and
//! its Android counterpart (178 lines of Kotlin), re-derived against three
//! COMPLETE file transfers captured in capture `eyevue_2`, capture `client_1` and
//! capture `client_2` — 79 frames, every one of which reassembles byte-for-byte into a JPEG
//! whose length equals the total the device declared.
//!
//! ```text
//!   info   52 58 | len (2, BIG-endian) | 97 | total (4 BE) type (1) | crc (1) | 58 52
//!   data   52 58 | len (2, BIG-endian) | 98 | addr  (4 BE) bytes…   | crc (1) | 58 52
//!   end    52 58 | len (2, BIG-endian) | 99 | 00                    | crc (1) | 58 52
//!
//!   len = 1 (cmd) + payload + 1 (crc)     so a whole frame is 4 + len + 2 bytes
//!   crc = (cmd + Σ payload) & 0xFF        the same additive checksum as the AB 55 envelope
//! ```
//!
//! ## Why this is not [`crate::frame`]
//!
//! The checksum arithmetic is identical — [`checksum`](crate::frame::checksum) is reused rather
//! than retyped, and so are [`MIN_LENGTH_FIELD`] and [`MAX_LENGTH_FIELD`], because `len` means
//! exactly the same thing in both. What differs is the envelope: a different magic, and a
//! two-byte TRAILER that `AB 55`/`AC 55` frames do not have. [`crate::frame::Deframer`]
//! deliberately refuses these bytes and hands them back as [`Residual::Foreign`], which is the
//! seam this module plugs into.
//!
//! [`Residual::Foreign`]: crate::frame::Residual::Foreign
//!
//! **The cmd bytes collide and the collision is real.** `0x97` and `0x99` mean *file info* and
//! *file end* here, and *voice-capture start* and *voice-capture end* inside an `AC 55` frame
//! (see [`crate::voice`]). Nothing in the byte distinguishes them — the envelope does, and so
//! does the characteristic. A router that keys on the cmd byte alone will feed a JPEG header
//! into the microphone path.
//!
//! ## What the captures show
//!
//! | | `eyevue_2` | `client_1` | `client_2` |
//! |---|---|---|---|
//! | declared total | 10,532 | 13,472 | 10,948 |
//! | type byte | `0x02` | `0x02` | `0x02` |
//! | full chunks | 21 × 496 | 27 × 496 | 22 × 496 |
//! | last chunk | 116 | 80 | 36 |
//! | reassembled | 10,532 ✓ | 13,472 ✓ | 10,948 ✓ |
//!
//! Every transfer is triggered by `ab 55 00 03 22 31 53` — [`TakePhoto`] with data `'1'`, the
//! "photo + HD image for AI" variant — and the JPEG is PUSHED over BLE about 2.1 s later. The
//! reassembled bytes start `ff d8` (JPEG SOI) in all three.
//!
//! [`TakePhoto`]: crate::AppCommand::TakePhoto
//!
//! Four things the captures establish that the reference implementations' comments do not:
//!
//! * **Delivery is one frame per ATT notification, whole and alone.** All 79 frames, zero
//!   coalesced, zero split — the same result [`crate::frame`] found for `AC 55`. The streaming
//!   deframer below is therefore robustness, not a transcription of observed behaviour, and is
//!   labelled that way rather than presented as device-confirmed.
//! * **`AA15` carried nothing but `52 58` in every capture.** File frames arrive on handle
//!   `0x0011`; `AC 55` control and voice frames arrive on `0x000e`. The reference iOS implementation's demux, which
//!   assumes the two interleave on `AA15`, is defending against something no trace has shown.
//!   It is still the right shape — this module accepts a byte stream and resyncs rather than
//!   assuming clean framing — but the justification is caution, not evidence.
//! * **Chunks arrive strictly in order, with no gap, no duplicate and no retransmit.** `addr`
//!   advances by exactly 496 every frame until the short tail. The out-of-order, duplicate and
//!   gap handling in both reference implementations is therefore **INFERRED**: the `addr` field makes it
//!   expressible, but no device has ever used it. It is kept because a lost notification is
//!   cheap to survive and expensive to mis-handle, and because dropping it would silently
//!   change behaviour on firmware nobody has captured.
//! * **`PROTOCOL.md` §11 has an arithmetic slip.** It reports this transfer as
//!   10,960 B from `22 × 496 + 48`, adding the last frame's 48 WIRE bytes instead of its 36
//!   payload bytes. `client_2`'s info frame declares `00 00 2a c4` = 10,948, and 22 × 496 + 36 =
//!   10,948 is what the reassembly actually produces.
//!
//! ## Divergences from the reference implementations, and why each one is a fix
//!
//! 1. **Android accepts a holey file as complete.** Its `0x99` check is
//!    `payload.size != declaredTotal`, and `payload` includes the zero-fill the gap branch just
//!    wrote — so a transfer that lost an interior frame passes, and a JPEG with a zeroed stripe
//!    is delivered as a successful capture. iOS counts only genuinely-delivered byte ranges.
//!    Here there is one counter, [`ReceivedRanges`], and no way to ask the other question.
//! 2. **Android stalls its own drain on a corrupt frame.** `nextFrame()` returns `null` after a
//!    bad crc, which breaks the `while` loop in `ingest` even though complete frames — possibly
//!    including the terminating `0x99` — are still sitting in the buffer. iOS fixed this; the
//!    reference Android implementation still has it. Pinned by a test below.
//! 3. **Neither reference implementation bounds `len`.** A corrupt length field larger than the buffer will ever
//!    hold makes both wait forever, exactly the wedge [`MAX_LENGTH_FIELD`] documents for the
//!    control channel. Here an unsatisfiable length resyncs.
//! 4. **No allocation is driven by a wire-supplied number.** iOS pre-reserves
//!    `min(declaredTotal, 16 MB)` and then range-checks each frame; Android does not check at
//!    all, so a crafted `addr` drives an unbounded zero-fill. Here the ceiling is enforced ONCE,
//!    when the `0x97` header arrives, and a transfer that declares more than
//!    [`MAX_FILE_BYTES`] never opens. Every later allocation is bounded by a total that has
//!    already been validated.
//! 5. **Data frames with no open transfer are dropped, not accumulated.** If the `0x97` is lost
//!    there is no declared size, so the `0x99` can only reject — both reference implementations buffer megabytes
//!    they are guaranteed to throw away. Same outcome, none of the work.
//! 6. **A failure is reported instead of swallowed.** Both reference implementations return an empty array and reset;
//!    from the outside "the transfer is 99 % across" and "the device never answered" looked
//!    identical, which is why a nearly-complete capture surfaced as "no file arrived from the
//!    glasses". [`FileAbort`] says which happened.
//! 7. **An aborted transfer no longer discards buffered wire bytes.** The Swift out-of-range
//!    branch calls its full `reset()`, which also empties the framing buffer — throwing away
//!    complete, correctly-framed frames that had already arrived. [`FileReassembler::reset`]
//!    (link teardown) and the internal transfer abort are separate here.
//!
//! ## Sans-IO
//!
//! This owns bytes and nothing else. It has no clock, so "the transfer went quiet" is not a
//! question it can answer — the shell times that and calls [`FileReassembler::reset`], per
//! client policy. Progress is a QUERY ([`FileReassembler::progress`]) rather than an event, because
//! a `0x98` arrives roughly 27 times a second and the shell coalesces on a timer it owns.

use crate::frame::{checksum, Frame, FRAME_OVERHEAD, MAX_LENGTH_FIELD, MIN_LENGTH_FIELD};
use crate::opcodes::Evidence;

/// File-frame magic, device → app.
pub const FILE_HEADER: [u8; 2] = [0x52, 0x58];

/// File-frame trailer — the magic reversed. `AB 55`/`AC 55` frames have no analogue.
pub const FILE_TRAILER: [u8; 2] = [0x58, 0x52];

/// magic (2) + length field (2) + trailer (2). The length field counts none of them.
pub const FILE_FRAME_OVERHEAD: usize = FRAME_OVERHEAD + 2;

/// The `0x98` chunk size every captured transfer uses, up to the short final frame.
///
/// Recorded because it is the one number that lets a reader check the geometry table in this
/// module's docs by hand. Nothing depends on it — a device that chunks differently reassembles
/// identically, since `addr` says where every byte goes.
pub const CAPTURED_CHUNK_BYTES: usize = 496;

/// Hard ceiling on a declared file size.
///
/// `total` is a 32-bit field read straight off the wire and the only integrity check on this
/// stream is an 8-bit additive checksum, which a flaky link can bit-flip past and a faulty
/// peripheral can craft. Enforced once, at the `0x97` header: a transfer declaring more than
/// this never opens, so no later allocation can be driven past it. 16 MiB comfortably covers
/// thumbnails and HD stills — the largest transfer in any capture is 13,472 bytes.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// The three cmd bytes that appear inside a `52 58` frame.
///
/// **These bytes are not unique to this framing.** `0x97` and `0x99` are also
/// [`DeviceUpload::VoiceUploadStart`] and [`DeviceUpload::VoiceUploadEnd`] inside an `AC 55`
/// frame, where they mean the microphone opened and closed. The envelope disambiguates; the
/// byte does not. That is why this is its own enum rather than a reuse of [`DeviceUpload`].
///
/// [`DeviceUpload`]: crate::DeviceUpload
/// [`DeviceUpload::VoiceUploadStart`]: crate::DeviceUpload::VoiceUploadStart
/// [`DeviceUpload::VoiceUploadEnd`]: crate::DeviceUpload::VoiceUploadEnd
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FileOpcode {
    /// `0x97` — `total (4 BE) type (1)`. Captured in all three transfers.
    Info = 0x97,
    /// `0x98` — `addr (4 BE) bytes…`. 76 captured frames.
    Data = 0x98,
    /// `0x99` — one `0x00` byte. Captured as `52 58 00 03 99 00 99 58 52`, identical in all
    /// three transfers.
    End = 0x99,
}

impl FileOpcode {
    /// Every file-stream cmd byte, ascending.
    pub const ALL: [FileOpcode; 3] = [FileOpcode::Info, FileOpcode::Data, FileOpcode::End];

    /// The wire byte.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Classify a cmd byte that arrived inside a `52 58` frame. `None` is genuinely unknown —
    /// unlike the `AC 55` channel, this framing carries no echoes.
    pub fn from_code(code: u8) -> Option<FileOpcode> {
        FileOpcode::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// The `0x97` header's type byte.
///
/// Only [`FileKind::HdImage`] has ever been seen. The other two come from both reference implementations and the
/// vendor PDF, and are marked [`Evidence::ClientOnly`] rather than presented as fact — a caller
/// branching on `ImageThumb` is branching on a value no device has sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FileKind {
    /// `0x01` — image thumbnail. CLIENT-ONLY.
    ImageThumb = 0x01,
    /// `0x02` — full-resolution still. The type byte in all three captured transfers.
    HdImage = 0x02,
    /// `0x03` — video thumbnail. CLIENT-ONLY.
    VideoThumb = 0x03,
}

impl FileKind {
    /// Every declared type byte, ascending.
    pub const ALL: [FileKind; 3] = [
        FileKind::ImageThumb,
        FileKind::HdImage,
        FileKind::VideoThumb,
    ];

    /// The wire byte.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Classify the `0x97` type byte, or `None` for a value no client declares.
    pub fn from_code(code: u8) -> Option<FileKind> {
        FileKind::ALL.into_iter().find(|k| k.code() == code)
    }

    /// What backs this value.
    pub fn evidence(self) -> Evidence {
        match self {
            FileKind::HdImage => Evidence::Capture,
            FileKind::ImageThumb | FileKind::VideoThumb => Evidence::ClientOnly,
        }
    }
}

/// Why a byte sequence is not a file frame.
///
/// Mirrors [`crate::frame::DecodeError`] on purpose, including the load-bearing split between
/// `BadLength` (never satisfiable — RESYNC) and `Truncated` (more bytes may fix it — WAIT).
/// It is a separate type only because this envelope has a trailer and that one does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFrameError {
    /// Shorter than the smallest legal frame: magic, length, cmd, crc, trailer.
    TooShort,
    /// The first two bytes are not [`FILE_HEADER`].
    BadHeader,
    /// The length field is below [`MIN_LENGTH_FIELD`] or above [`MAX_LENGTH_FIELD`].
    BadLength,
    /// The length is plausible but the buffer is shorter than the frame it describes.
    Truncated,
    /// The last two bytes are not [`FILE_TRAILER`].
    BadTrailer,
    /// `(cmd + Σ payload) & 0xFF` does not match the byte before the trailer.
    ChecksumMismatch,
}

/// Build one `52 58` frame.
///
/// This crate never plays device on a radio; this exists so tests and fixtures can construct
/// inbound bytes, and so a captured frame can be checked by re-encoding it rather than only by
/// decoding it. Note the absence of the `AB 55` envelope's mandatory `0x00` empty-payload
/// filler: that is a rule about app → device COMMANDS (protocol §2.1.1) and has no bearing
/// here — the `0x99` end frame's single `0x00` is a real payload byte, not a filler.
pub fn encode_file_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let len = 1 + payload.len() + 1; // cmd + payload + checksum
    let mut out = Vec::with_capacity(FILE_FRAME_OVERHEAD + len);
    out.extend_from_slice(&FILE_HEADER);
    out.push((len >> 8) as u8);
    out.push((len & 0xFF) as u8);
    out.push(cmd);
    out.extend_from_slice(payload);
    out.push(checksum(cmd, payload));
    out.extend_from_slice(&FILE_TRAILER);
    out
}

/// Decode exactly one `52 58` frame from the front of `bytes`. Trailing bytes are ignored.
///
/// The decoded shape is a cmd byte and a payload, which is precisely [`Frame`] — so this
/// returns that type rather than declaring a near-identical twin of it. Only the envelope
/// differs between the two framings, and the envelope is not part of the decoded value.
///
/// The trailer is checked before the checksum: a wrong trailer means the length field pointed
/// at the wrong place, which is structural, while a wrong checksum means the length was right
/// and the contents were not. The distinction drives what the deframer does next.
pub fn decode_file_frame(bytes: &[u8]) -> Result<Frame, FileFrameError> {
    if bytes.len() < FILE_FRAME_OVERHEAD + MIN_LENGTH_FIELD {
        return Err(FileFrameError::TooShort);
    }
    if bytes[..2] != FILE_HEADER {
        return Err(FileFrameError::BadHeader);
    }
    let len = ((bytes[2] as usize) << 8) | bytes[3] as usize;
    if !(MIN_LENGTH_FIELD..=MAX_LENGTH_FIELD).contains(&len) {
        return Err(FileFrameError::BadLength);
    }
    let total = FRAME_OVERHEAD + len + FILE_TRAILER.len();
    if bytes.len() < total {
        return Err(FileFrameError::Truncated);
    }
    if bytes[total - 2..total] != FILE_TRAILER {
        return Err(FileFrameError::BadTrailer);
    }
    let cmd = bytes[4];
    let payload = &bytes[5..FRAME_OVERHEAD + len - 1];
    if bytes[FRAME_OVERHEAD + len - 1] != checksum(cmd, payload) {
        return Err(FileFrameError::ChecksumMismatch);
    }
    Ok(Frame::new(cmd, payload))
}

/// Decode as many complete file frames as `buffer` holds, consuming them in place.
///
/// Returns the frames and how many bytes had to be discarded to regain sync. The remainder is
/// left alone: either a trailing partial frame, or a lone `0x52` that could still become a
/// header.
///
/// Three policies, each chosen for a reason the `AB 55` deframer states at length:
///
/// * A frame whose checksum or trailer is wrong is CONSUMED AND DROPPED. The length field
///   already located its end, so the stream stays aligned; resyncing past it would land inside
///   the NEXT frame and turn one corrupt packet into a cascade. The loop then KEEPS GOING —
///   this is the reference Android implementation's stall, where a bad frame stopped the drain and stranded a
///   `0x99` that had already arrived.
/// * An unsatisfiable length drops the two magic bytes and re-tests, rather than waiting for
///   bytes that will never come.
/// * Leading bytes that cannot begin a frame are scanned past to the next `52 58`. This is
///   where the file stream deliberately differs from [`crate::frame::drain`], which refuses to
///   scan forward: that deframer shares `AA15` with this framing and a forward scan there would
///   manufacture a control frame out of JPEG bytes. By the time bytes reach here the caller has
///   already demultiplexed, everything in the buffer is meant to be a file frame, and dropping
///   to the next header is the only way back from a desync.
pub fn drain_file_frames(buffer: &mut Vec<u8>) -> (Vec<Frame>, usize) {
    let mut out = Vec::new();
    let mut dropped = 0;
    loop {
        dropped += resync_to_header(buffer);
        if buffer.len() < FRAME_OVERHEAD {
            break; // magic or length field not fully arrived — wait
        }
        let len = ((buffer[2] as usize) << 8) | buffer[3] as usize;
        if !(MIN_LENGTH_FIELD..=MAX_LENGTH_FIELD).contains(&len) {
            buffer.drain(..2);
            dropped += 2;
            continue;
        }
        let total = FRAME_OVERHEAD + len + FILE_TRAILER.len();
        if buffer.len() < total {
            break; // partial frame — wait for the next notification
        }
        if let Ok(frame) = decode_file_frame(&buffer[..total]) {
            out.push(frame);
        }
        buffer.drain(..total);
    }
    // The length cap bounds the buffer by itself: whatever is left starts with a header (or is
    // a lone `0x52`), and a header with a legal length describes at most this many bytes — so a
    // partial frame can never grow past it and absorb the channel. That is why there is no
    // `Overflowed` case here and there is one on the control channel, which cannot resync.
    debug_assert!(buffer.len() <= FILE_FRAME_OVERHEAD + MAX_LENGTH_FIELD);
    (out, dropped)
}

/// Drop leading bytes until the buffer starts with [`FILE_HEADER`]; returns how many went.
///
/// A trailing lone `0x52` is KEPT — it is half a header split across notifications, and
/// discarding it would corrupt the frame that is arriving.
fn resync_to_header(buffer: &mut Vec<u8>) -> usize {
    if buffer.starts_with(&FILE_HEADER) || buffer.is_empty() {
        return 0;
    }
    match buffer.windows(2).position(|w| w == FILE_HEADER) {
        Some(at) => {
            buffer.drain(..at);
            at
        }
        None => {
            let keep = usize::from(buffer.last() == Some(&FILE_HEADER[0]));
            let n = buffer.len() - keep;
            buffer.drain(..n);
            n
        }
    }
}

/// A completed file, exactly as many bytes as its `0x97` header declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFile {
    /// The raw `0x97` type byte, kept raw because the wire is the authority on it.
    pub file_type: u8,
    /// The reassembled contents.
    pub data: Vec<u8>,
}

impl ReassembledFile {
    /// The type byte as a named value, or `None` for one no client declares.
    pub fn kind(&self) -> Option<FileKind> {
        FileKind::from_code(self.file_type)
    }
}

/// Why a transfer ended without producing a file.
///
/// Both reference implementations return an empty list in every one of these cases, which is why a photo that was
/// most of the way across and a device that never answered were indistinguishable from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAbort {
    /// A `0x99` arrived with no transfer open — the `0x97` header was lost or corrupt, so there
    /// is no declared size to check against and no type byte that is not stale.
    NoFileInfo,
    /// A `0x97` declared a size of zero, or more than [`MAX_FILE_BYTES`]. The transfer never
    /// opened, so nothing was allocated on the strength of it.
    ImplausibleSize { declared_total: usize },
    /// A `0x97` arrived while a transfer was still in flight. Both reference implementations restart silently and
    /// the partial file evaporates.
    Superseded {
        received: usize,
        declared_total: usize,
    },
    /// A `0x98` addressed bytes outside the declared file. The stream is not what the header
    /// said it would be, so the transfer is closed rather than grown to fit.
    OutOfRange {
        addr: usize,
        end: usize,
        declared_total: usize,
    },
    /// A `0x99` arrived with fewer bytes GENUINELY delivered than declared. `first_gap` is the
    /// offset of the first byte never received — `None` would mean the tail is simply missing,
    /// which cannot happen here because a short tail is itself a gap below `declared_total`.
    ///
    /// Also emitted mid-transfer, without waiting for the `0x99`, when the delivered bytes
    /// have fragmented into more than `MAX_RECEIVED_SPANS` disjoint spans: every span is a
    /// hole, so the file cannot complete, and continuing to track them is unbounded work on a
    /// wire-supplied count.
    Incomplete {
        received: usize,
        declared_total: usize,
        first_gap: Option<usize>,
    },
}

/// What a chunk of `AA15` bytes did to the transfer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// A `0x97` header opened a transfer.
    Started {
        declared_total: usize,
        file_type: u8,
    },
    /// A `0x99` closed one, and every declared byte was genuinely delivered.
    Completed(ReassembledFile),
    /// The transfer ended with nothing to hand over.
    Aborted(FileAbort),
    /// Bytes that could not begin a frame were discarded to regain sync. Surfaced rather than
    /// swallowed because on a channel that carries only this framing, a desync is a symptom.
    Desynced { dropped: usize },
}

/// How far along the in-flight transfer is.
///
/// `received` counts bytes a `0x98` genuinely delivered and never the gap branch's zero-fill,
/// so it cannot be inflated by padding — which is the whole difference between the two reference implementations'
/// completeness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferProgress {
    pub received: usize,
    pub declared_total: usize,
    pub file_type: u8,
    /// Offset of the first byte not yet delivered, or `None` when the file is complete. On the
    /// strictly-in-order stream every capture shows, this is simply the write cursor.
    pub first_gap: Option<usize>,
}

impl TransferProgress {
    /// Whether a `0x97` has opened a transfer that has not yet closed.
    pub fn is_active(&self) -> bool {
        self.declared_total > 0
    }
}

/// Streaming accumulator for the `52 58` file channel.
///
/// Feed it whatever a notification delivered; take completed files (and failures) out. It owns
/// two pieces of state — the framing buffer and the in-flight transfer — and the distinction
/// matters: a transfer can be abandoned without discarding correctly-framed bytes that have
/// already arrived, which the reference iOS implementation's shared `reset()` could not express.
#[derive(Debug, Clone, Default)]
pub struct FileReassembler {
    buffer: Vec<u8>,
    transfer: Option<Transfer>,
}

#[derive(Debug, Clone)]
struct Transfer {
    declared_total: usize,
    file_type: u8,
    /// Sized only by bytes that actually arrived; never pre-reserved from a wire-supplied count.
    data: Vec<u8>,
    received: ReceivedRanges,
}

impl FileReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes held that have not yet formed a frame.
    pub fn buffered(&self) -> &[u8] {
        &self.buffer
    }

    /// Whether a partial frame is buffered. The iOS demux uses this to decide whether an
    /// `AC 55`-looking chunk is a genuine control frame or JPEG bytes that happen to start
    /// with the magic.
    pub fn has_buffered_frame_data(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// How far along the in-flight transfer is; all zeroes when none is open.
    pub fn progress(&self) -> TransferProgress {
        match &self.transfer {
            None => TransferProgress::default(),
            Some(t) => TransferProgress {
                received: t.received.covered(),
                declared_total: t.declared_total,
                file_type: t.file_type,
                first_gap: t.received.first_gap(t.declared_total),
            },
        }
    }

    /// Forget everything: buffered bytes and any transfer in flight.
    ///
    /// The shell calls this on disconnect, where a half-frame from the previous link would
    /// otherwise prefix the next one, and on its own transfer timeout — this module has no
    /// clock and cannot notice that the device went quiet (client policy).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.transfer = None;
    }

    /// Feed one notification's bytes; returns what happened.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<FileEvent> {
        self.buffer.extend_from_slice(chunk);
        let (frames, dropped) = drain_file_frames(&mut self.buffer);
        let mut events = Vec::new();
        if dropped > 0 {
            events.push(FileEvent::Desynced { dropped });
        }
        for frame in frames {
            self.handle(&frame, &mut events);
        }
        events
    }

    /// Route one already-decoded file frame. Exposed so a shell that does its own framing (or a
    /// test replaying captured frames) does not have to re-serialise them first.
    pub fn handle_frame(&mut self, frame: &Frame) -> Vec<FileEvent> {
        let mut events = Vec::new();
        self.handle(frame, &mut events);
        events
    }

    fn handle(&mut self, frame: &Frame, events: &mut Vec<FileEvent>) {
        match FileOpcode::from_code(frame.cmd) {
            Some(FileOpcode::Info) => self.on_info(&frame.data, events),
            Some(FileOpcode::Data) => self.on_data(&frame.data, events),
            Some(FileOpcode::End) => self.on_end(events),
            None => {} // no echoes on this framing; an unknown cmd is simply not ours
        }
    }

    fn on_info(&mut self, payload: &[u8], events: &mut Vec<FileEvent>) {
        // `total (4 BE) type (1)`. Both reference implementations require 5 bytes and ignore anything shorter.
        if payload.len() < 5 {
            return;
        }
        let declared_total =
            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let file_type = payload[4];

        if let Some(t) = self.transfer.take() {
            let received = t.received.covered();
            if received > 0 {
                events.push(FileEvent::Aborted(FileAbort::Superseded {
                    received,
                    declared_total: t.declared_total,
                }));
            }
        }

        // The ONLY place the ceiling is enforced. Refusing here rather than clamping is what
        // makes every later allocation bounded by an already-validated number.
        if declared_total == 0 || declared_total > MAX_FILE_BYTES {
            events.push(FileEvent::Aborted(FileAbort::ImplausibleSize {
                declared_total,
            }));
            return;
        }

        self.transfer = Some(Transfer {
            declared_total,
            file_type,
            data: Vec::new(),
            received: ReceivedRanges::default(),
        });
        events.push(FileEvent::Started {
            declared_total,
            file_type,
        });
    }

    fn on_data(&mut self, payload: &[u8], events: &mut Vec<FileEvent>) {
        if payload.len() < 4 {
            return;
        }
        // No open transfer means the `0x97` was lost, so the `0x99` can only reject. Both reference implementations
        // buffer the bytes anyway; dropping them reaches the same outcome without the work, and
        // without letting a wire-supplied `addr` size an allocation.
        let Some(declared_total) = self.transfer.as_ref().map(|t| t.declared_total) else {
            return;
        };
        let addr = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let chunk = &payload[4..];
        // `checked_add` rather than `+`: `addr` spans the full u32 range, and on a 32-bit
        // Android target `usize` is exactly that wide, so a crafted address near the top plus a
        // 496-byte chunk wraps.
        match addr.checked_add(chunk.len()) {
            Some(end) if end <= declared_total => {
                if chunk.is_empty() {
                    return;
                }
                let t = self.transfer.as_mut().expect("checked immediately above");
                // One write covers all three cases the reference implementations spell out separately — append at
                // the cursor, overwrite a retransmit, or zero-fill across a gap and write past.
                if end > t.data.len() {
                    t.data.resize(end, 0);
                }
                t.data[addr..end].copy_from_slice(chunk);
                // Only the genuinely-delivered range is recorded. The zero-fill above is
                // padding, and counting it is how Android accepts a holey JPEG as a whole one.
                if !t.received.insert(addr, end) {
                    // Too many holes to keep tracking. A file this fragmented cannot complete
                    // — every span is a gap — so ending it now reports the same outcome the
                    // `0x99` would have, without the unbounded bookkeeping to get there.
                    let received = t.received.covered();
                    let first_gap = t.received.first_gap(declared_total);
                    self.transfer = None;
                    events.push(FileEvent::Aborted(FileAbort::Incomplete {
                        received,
                        declared_total,
                        first_gap,
                    }));
                }
            }
            beyond => {
                self.transfer = None;
                events.push(FileEvent::Aborted(FileAbort::OutOfRange {
                    addr,
                    end: beyond.unwrap_or(usize::MAX),
                    declared_total,
                }));
            }
        }
    }

    fn on_end(&mut self, events: &mut Vec<FileEvent>) {
        let Some(mut t) = self.transfer.take() else {
            events.push(FileEvent::Aborted(FileAbort::NoFileInfo));
            return;
        };
        let received = t.received.covered();
        if received != t.declared_total {
            events.push(FileEvent::Aborted(FileAbort::Incomplete {
                received,
                declared_total: t.declared_total,
                first_gap: t.received.first_gap(t.declared_total),
            }));
            return;
        }
        // `received == declared_total` with every range inside `[0, declared_total)` means the
        // ranges tile the file exactly, so the buffer is already the right length — but a device
        // that stopped short of its own declaration would leave it shorter, and handing back a
        // buffer that disagrees with the count we just checked is not worth the saved line.
        t.data.resize(t.declared_total, 0);
        events.push(FileEvent::Completed(ReassembledFile {
            file_type: t.file_type,
            data: t.data,
        }));
    }
}

/// The set of byte offsets a `0x98` frame actually delivered.
///
/// Sorted, disjoint, non-adjacent half-open spans. It exists because the completeness check has
/// to distinguish "the file is 10,532 bytes long" from "10,532 bytes were received": the gap
/// branch writes zeroes to keep later offsets correct, and those zeroes must not count. Swift
/// used `IndexSet` for this; Kotlin had no equivalent and used the buffer length instead, which
/// is the holey-file bug.
///
/// In the observed strictly-in-order stream this holds exactly one span, so the linear merge
/// below is doing nothing on the hot path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReceivedRanges {
    spans: Vec<(usize, usize)>,
}

/// How many disjoint spans one transfer may accumulate before it is abandoned.
///
/// The observed stream is strictly in order and holds ONE span, so this is unreachable on any
/// real transfer. It exists because the span count is otherwise driven straight off the wire: a
/// device (or a corrupted link) that delivers every other 496-byte chunk of a 16 MB file makes
/// one span per chunk, and the linear merge in [`ReceivedRanges::insert`] is O(n) per chunk —
/// so an adversarial or badly-broken stream buys quadratic work and an unbounded `Vec` for a
/// file that can never complete anyway. 4096 is far above the ~33k chunks a maximum-size file
/// has when in order (1 span) and far above any plausible retransmit pattern.
const MAX_RECEIVED_SPANS: usize = 4096;

impl ReceivedRanges {
    /// Returns `false` when the insert would push the span count past
    /// [`MAX_RECEIVED_SPANS`]; the caller abandons the transfer in that case. The span is not
    /// recorded when this returns `false`, which does not matter — the transfer is over.
    #[must_use]
    fn insert(&mut self, start: usize, end: usize) -> bool {
        if start >= end {
            return true;
        }
        // Checked BEFORE the merge, because the merge is the expensive half. A span that fuses
        // with a neighbour does not grow the count, so this only ever rejects a genuinely new
        // hole — which is why an in-order stream never sees it.
        if self.spans.len() >= MAX_RECEIVED_SPANS {
            return false;
        }
        let (mut lo, mut hi) = (start, end);
        let mut merged = Vec::with_capacity(self.spans.len() + 1);
        let mut placed = false;
        for &(s, e) in &self.spans {
            if e < lo {
                merged.push((s, e)); // ends before the new span begins, and does not touch it
            } else if s > hi {
                if !placed {
                    merged.push((lo, hi));
                    placed = true;
                }
                merged.push((s, e));
            } else {
                lo = lo.min(s); // overlaps or abuts — absorb
                hi = hi.max(e);
            }
        }
        if !placed {
            merged.push((lo, hi));
        }
        self.spans = merged;
        true
    }

    fn covered(&self) -> usize {
        self.spans.iter().map(|(s, e)| e - s).sum()
    }

    /// The first offset below `total` that no frame delivered, or `None` when the range
    /// `[0, total)` is fully covered.
    fn first_gap(&self, total: usize) -> Option<usize> {
        let mut cursor = 0;
        for &(s, e) in &self.spans {
            if s > cursor {
                return Some(cursor);
            }
            cursor = cursor.max(e);
        }
        (cursor < total).then_some(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            s.len().is_multiple_of(2),
            "hex literal must be byte-aligned"
        );
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Frames lifted verbatim out of the packet captures, read off the wire with
    /// `tshark -r <capture>.pklg -Y btatt.value`. A decode that agrees with both reference implementations but
    /// disagrees with one of these is wrong, not merely different.
    const GOLDEN_INFO: &[(&str, usize, u8, &str)] = &[
        (
            "525800079700002ac402875852",
            10_948,
            0x02,
            "client_2 — 22 x 496 + 36; PROTOCOL.md §11 records 10,960 for this transfer by adding the \
             last frame's 48 WIRE bytes instead of its 36 payload bytes",
        ),
        ("52580007970000292402e65852", 10_532, 0x02, "eyevue_2 — 21 x 496 + 116"),
        ("5258000797000034a0026d5852", 13_472, 0x02, "client_1 — 27 x 496 + 80, the largest seen"),
    ];

    /// `client_2`'s final `0x98`, whole and unedited: the short tail that closes the file at
    /// exactly the declared total. 48 wire bytes — the smallest complete real data frame in any
    /// capture, which is why it is the one embedded here.
    const GOLDEN_LAST_DATA: &str = "5258002a9800002aa05ffe08ccb78bfdafc047ff00db68317a363d498b\
                                    ff00b6d6a514ff00adff00e08cffd900ef5852";

    /// The end frame, byte-identical in all three captured transfers.
    const GOLDEN_END: &str = "525800039900995852";

    /// Build a whole transfer at the geometry the captures use. Content is deterministic and
    /// content-addressed by offset so a misplaced chunk shows up as a byte mismatch rather than
    /// as a length that happens to match.
    fn transfer(total: usize, file_type: u8) -> (Vec<u8>, Vec<Vec<u8>>) {
        let body: Vec<u8> = (0..total)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect();
        let mut frames = vec![{
            let mut p = (total as u32).to_be_bytes().to_vec();
            p.push(file_type);
            encode_file_frame(FileOpcode::Info.code(), &p)
        }];
        for addr in (0..total).step_by(CAPTURED_CHUNK_BYTES) {
            let end = (addr + CAPTURED_CHUNK_BYTES).min(total);
            let mut p = (addr as u32).to_be_bytes().to_vec();
            p.extend_from_slice(&body[addr..end]);
            frames.push(encode_file_frame(FileOpcode::Data.code(), &p));
        }
        frames.push(encode_file_frame(FileOpcode::End.code(), &[0x00]));
        (body, frames)
    }

    fn completed(events: &[FileEvent]) -> Option<&ReassembledFile> {
        events.iter().find_map(|e| match e {
            FileEvent::Completed(f) => Some(f),
            _ => None,
        })
    }

    fn aborts(events: &[FileEvent]) -> Vec<FileAbort> {
        events
            .iter()
            .filter_map(|e| match e {
                FileEvent::Aborted(a) => Some(*a),
                _ => None,
            })
            .collect()
    }

    /// The load-bearing test: every captured header decodes to the total and type the transfer
    /// actually produced, AND re-encodes to the original bytes. Round-tripping in both
    /// directions is what pins the length and checksum boundaries — a codec that counted the
    /// trailer inside `len`, or ran the checksum over the header, passes a decode-only test on
    /// the `0x99` frame and fails here on the first multi-byte one.
    #[test]
    fn every_captured_file_frame_decodes_and_re_encodes_byte_for_byte() {
        for (h, total, kind, why) in GOLDEN_INFO {
            let bytes = hex(h);
            let f = decode_file_frame(&bytes).unwrap_or_else(|e| panic!("{h} ({why}): {e:?}"));
            assert_eq!(f.cmd, FileOpcode::Info.code(), "{why}");
            assert_eq!(f.data.len(), 5, "{why}");
            let got = u32::from_be_bytes([f.data[0], f.data[1], f.data[2], f.data[3]]) as usize;
            assert_eq!(got, *total, "{why}");
            assert_eq!(f.data[4], *kind, "{why}");
            assert_eq!(encode_file_frame(f.cmd, &f.data), bytes, "{why}");
        }

        let last = hex(GOLDEN_LAST_DATA);
        let f = decode_file_frame(&last).expect("the captured short tail decodes");
        assert_eq!(f.cmd, FileOpcode::Data.code());
        let addr = u32::from_be_bytes([f.data[0], f.data[1], f.data[2], f.data[3]]) as usize;
        assert_eq!(
            (addr, f.data.len() - 4),
            (10_912, 36),
            "22 x 496, then 36 bytes"
        );
        assert_eq!(
            addr + (f.data.len() - 4),
            10_948,
            "the tail lands exactly on the total"
        );
        assert_eq!(encode_file_frame(f.cmd, &f.data), last);

        let end = hex(GOLDEN_END);
        let f = decode_file_frame(&end).expect("the captured end frame decodes");
        assert_eq!(
            (f.cmd, f.data.as_slice()),
            (FileOpcode::End.code(), &[0x00][..])
        );
        assert_eq!(encode_file_frame(f.cmd, &f.data), end);
    }

    /// `len` excludes the header AND the trailer; the checksum covers cmd + payload only.
    /// Asserted on a captured frame with a multi-byte payload, where every boundary is visible.
    #[test]
    fn length_and_checksum_boundaries_are_where_the_captures_put_them() {
        let bytes = hex("525800079700002ac402875852");
        let len = ((bytes[2] as usize) << 8) | bytes[3] as usize;
        assert_eq!(len, 7, "cmd + 5 payload + checksum");
        assert_eq!(
            bytes.len(),
            FRAME_OVERHEAD + len + 2,
            "magic and trailer are outside len"
        );
        assert_eq!(checksum(0x97, &bytes[5..10]), 0x87);
        assert_eq!(bytes[10], 0x87);
        assert_eq!(&bytes[11..], &FILE_TRAILER);
        // Folding the header in would give a different byte — proof the boundary matters.
        assert_ne!(checksum(0x52, &bytes[1..10]), 0x87);
    }

    /// A full transfer at each captured geometry, driven one notification per frame, which is
    /// how the device actually delivers them.
    #[test]
    fn the_captured_transfer_geometries_reassemble_to_the_declared_total() {
        for (total, chunks, tail) in [
            (10_532usize, 21usize, 116usize),
            (13_472, 27, 80),
            (10_948, 22, 36),
        ] {
            assert_eq!(
                chunks * CAPTURED_CHUNK_BYTES + tail,
                total,
                "geometry table"
            );
            let (body, frames) = transfer(total, FileKind::HdImage.code());
            assert_eq!(frames.len(), chunks + 3, "info + data frames + end");

            let mut r = FileReassembler::new();
            let mut events = Vec::new();
            for f in &frames {
                events.extend(r.push(f));
            }
            assert_eq!(
                events.first(),
                Some(&FileEvent::Started {
                    declared_total: total,
                    file_type: 0x02
                })
            );
            let file = completed(&events).unwrap_or_else(|| panic!("{total} did not complete"));
            assert_eq!(file.data, body);
            assert_eq!(file.kind(), Some(FileKind::HdImage));
            assert!(r.progress().declared_total == 0, "the transfer closed");
            assert!(!r.has_buffered_frame_data());
        }
    }

    /// Delivery in every capture is one whole frame per notification. This asserts the
    /// deframer survives the coalescing and fragmentation nobody has observed — labelled as
    /// robustness so it is never cited as device evidence.
    #[test]
    fn a_transfer_survives_being_coalesced_and_split_arbitrarily() {
        let (body, frames) = transfer(2_000, 0x02);
        let stream: Vec<u8> = frames.concat();

        // Everything in one notification.
        let mut r = FileReassembler::new();
        let events = r.push(&stream);
        assert_eq!(
            completed(&events).map(|f| f.data.clone()),
            Some(body.clone())
        );

        // One byte at a time.
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for b in &stream {
            events.extend(r.push(&[*b]));
        }
        assert_eq!(
            completed(&events).map(|f| f.data.clone()),
            Some(body.clone())
        );

        // And split at every possible boundary.
        for cut in 1..stream.len() {
            let mut r = FileReassembler::new();
            let mut events = r.push(&stream[..cut]);
            events.extend(r.push(&stream[cut..]));
            assert_eq!(
                completed(&events).map(|f| f.data.len()),
                Some(body.len()),
                "split at {cut}"
            );
        }
    }

    /// A dropped chunk must NOT be reported as a complete file. This is the live Android bug:
    /// its check is `payload.size != declaredTotal`, and the gap branch already grew `payload`
    /// with zeroes — so a JPEG with a zeroed stripe passes as a successful capture.
    #[test]
    fn a_dropped_chunk_is_rejected_rather_than_delivered_with_a_hole() {
        let (_, frames) = transfer(2_000, 0x02);
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if i == 2 {
                continue; // lose the second data frame
            }
            events.extend(r.push(f));
        }
        assert!(
            completed(&events).is_none(),
            "a holey file must not be delivered"
        );
        assert_eq!(
            aborts(&events),
            vec![FileAbort::Incomplete {
                received: 2_000 - CAPTURED_CHUNK_BYTES,
                declared_total: 2_000,
                first_gap: Some(CAPTURED_CHUNK_BYTES),
            }],
            "the gap is reported, and it is reported where it is"
        );
    }

    /// The same gap, filled by a late retransmit. `addr` is what makes this expressible; no
    /// device has ever used it, so this pins INFERRED behaviour rather than captured behaviour.
    #[test]
    fn a_late_retransmit_fills_the_gap_and_the_file_completes() {
        let (body, frames) = transfer(2_000, 0x02);
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if i == 2 {
                continue;
            }
            events.extend(r.push(f));
        }
        // The zero-fill left the buffer the right LENGTH but the wrong CONTENT, and the gap is
        // still outstanding — so progress reports it even though `data.len()` is already 2,000.
        assert!(completed(&events).is_none());

        // Replay the whole transfer, this time delivering the missing chunk last.
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if i == 2 || i == frames.len() - 1 {
                continue;
            }
            events.extend(r.push(f));
        }
        assert_eq!(r.progress().first_gap, Some(CAPTURED_CHUNK_BYTES));
        events.extend(r.push(&frames[2]));
        assert_eq!(r.progress().first_gap, None, "the hole closed");
        events.extend(r.push(&frames[frames.len() - 1]));
        assert_eq!(completed(&events).map(|f| f.data.clone()), Some(body));
    }

    /// Duplicates must not double-count, and out-of-order arrival must not shift a byte.
    /// Both are INFERRED paths — every captured transfer is strictly ascending and gapless.
    #[test]
    fn duplicates_and_out_of_order_chunks_reassemble_correctly() {
        let (body, frames) = transfer(2_000, 0x02);
        let info = &frames[0];
        let end = &frames[frames.len() - 1];
        let data = &frames[1..frames.len() - 1];

        let mut r = FileReassembler::new();
        let mut events = r.push(info);
        // Reverse order, with every chunk sent twice.
        for f in data.iter().rev() {
            events.extend(r.push(f));
            events.extend(r.push(f));
        }
        assert_eq!(
            r.progress().received,
            2_000,
            "a duplicate adds no bytes to the count"
        );
        events.extend(r.push(end));
        assert_eq!(completed(&events).map(|f| f.data.clone()), Some(body));
    }

    /// A truncated transfer — the device stops before the tail and then closes.
    #[test]
    fn a_truncated_tail_is_reported_with_where_it_stopped() {
        let (_, frames) = transfer(2_000, 0x02);
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for f in &frames[..frames.len() - 2] {
            events.extend(r.push(f));
        }
        events.extend(r.push(&frames[frames.len() - 1]));
        assert_eq!(
            aborts(&events),
            vec![FileAbort::Incomplete {
                received: 4 * CAPTURED_CHUNK_BYTES,
                declared_total: 2_000,
                first_gap: Some(4 * CAPTURED_CHUNK_BYTES),
            }]
        );
    }

    /// The reference Android implementation's stall, pinned. A corrupt frame made `nextFrame()` return `null`, which
    /// broke the drain loop — so a `0x99` coalesced into the SAME notification was stranded
    /// until the next one, which on a finished transfer never comes. The frame is consumed and
    /// dropped; draining continues.
    #[test]
    fn a_corrupt_frame_does_not_strand_the_end_frame_behind_it() {
        let (body, frames) = transfer(1_000, 0x02);
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for f in &frames[..frames.len() - 1] {
            events.extend(r.push(f));
        }
        // A checksum-corrupt frame immediately followed by the real end frame, in one chunk.
        let mut corrupt = encode_file_frame(FileOpcode::Data.code(), &[0, 0, 0, 0, 0xAA]);
        let n = corrupt.len();
        corrupt[n - 3] ^= 0xFF;
        let mut coalesced = corrupt.clone();
        coalesced.extend_from_slice(&frames[frames.len() - 1]);

        assert_eq!(
            decode_file_frame(&corrupt),
            Err(FileFrameError::ChecksumMismatch)
        );
        events.extend(r.push(&coalesced));
        assert_eq!(
            completed(&events).map(|f| f.data.clone()),
            Some(body),
            "the end frame behind the corrupt one must still be seen"
        );
    }

    /// A bad trailer is structural — the length pointed at the wrong place — and is caught
    /// before the checksum so the two failures stay distinguishable.
    #[test]
    fn a_bad_trailer_is_its_own_failure_and_the_frame_is_still_consumed() {
        let mut f = encode_file_frame(FileOpcode::End.code(), &[0x00]);
        *f.last_mut().expect("non-empty") = 0x00;
        assert_eq!(decode_file_frame(&f), Err(FileFrameError::BadTrailer));

        let good = encode_file_frame(FileOpcode::Info.code(), &[0, 0, 0, 10, 0x02]);
        let mut stream = f;
        stream.extend_from_slice(&good);
        let mut r = FileReassembler::new();
        let events = r.push(&stream);
        assert_eq!(
            events,
            vec![FileEvent::Started {
                declared_total: 10,
                file_type: 0x02
            }],
            "the bad frame is dropped and the good one behind it still lands"
        );
        assert!(!r.has_buffered_frame_data());
    }

    /// An unsatisfiable length must release the buffer instead of waiting on bytes that will
    /// never arrive. Neither reference implementation bounds `len` at all, which is the same wedge
    /// `MAX_LENGTH_FIELD` documents for the control channel.
    #[test]
    fn an_oversized_length_resyncs_instead_of_waiting_forever() {
        let mut stream = vec![0x52, 0x58, 0xFF, 0xFF]; // len = 65535
        stream.extend_from_slice(&encode_file_frame(
            FileOpcode::Info.code(),
            &[0, 0, 0, 4, 0x02],
        ));

        let mut r = FileReassembler::new();
        let events = r.push(&stream);
        assert!(
            matches!(events.first(), Some(FileEvent::Desynced { .. })),
            "{events:?}"
        );
        assert!(
            events.contains(&FileEvent::Started {
                declared_total: 4,
                file_type: 0x02
            }),
            "one bad length costs a resync, not the rest of the stream: {events:?}"
        );
        assert!(!r.has_buffered_frame_data());

        assert_eq!(
            decode_file_frame(&[0x52, 0x58, 0xFF, 0xFF, 0, 0, 0, 0]),
            Err(FileFrameError::BadLength)
        );
        assert_eq!(
            decode_file_frame(&[0x52, 0x58, 0x00, 0x01, 0, 0, 0, 0]),
            Err(FileFrameError::BadLength),
            "len < 2 cannot describe even a cmd and a checksum"
        );
    }

    /// Leading garbage is scanned past — and a lone `0x52` at the end of a notification is a
    /// split header, not garbage, so it must survive to be completed by the next one.
    #[test]
    fn leading_garbage_resyncs_and_a_split_header_is_kept() {
        let good = encode_file_frame(FileOpcode::End.code(), &[0x00]);
        let mut stream = vec![0xDE, 0xAD, 0xBE, 0xEF];
        stream.extend_from_slice(&good);

        let mut r = FileReassembler::new();
        let events = r.push(&stream);
        assert_eq!(events.first(), Some(&FileEvent::Desynced { dropped: 4 }));

        let mut r = FileReassembler::new();
        assert_eq!(
            r.push(&[0x00, 0x52]),
            vec![FileEvent::Desynced { dropped: 1 }]
        );
        assert_eq!(
            r.buffered(),
            &[0x52],
            "the trailing 0x52 is half a header, not junk"
        );
        let events = r.push(&good[1..]);
        assert_eq!(
            aborts(&events),
            vec![FileAbort::NoFileInfo],
            "the frame completed"
        );
        assert!(!r.has_buffered_frame_data());
    }

    /// The buffer cannot grow without bound even under a flood of junk: the length cap plus the
    /// resync mean a partial frame can never outlast the largest frame the wire can describe.
    /// This is why there is no `Overflowed` case here and there is one on the control channel.
    #[test]
    fn the_framing_buffer_stays_bounded_under_junk() {
        let mut r = FileReassembler::new();
        for _ in 0..64 {
            r.push(&[0x52; 512]);
            assert!(r.buffered().len() <= FILE_FRAME_OVERHEAD + MAX_LENGTH_FIELD);
        }
        // …and a real frame after the flood still lands.
        let events = r.push(&encode_file_frame(
            FileOpcode::Info.code(),
            &[0, 0, 0, 4, 0x02],
        ));
        assert!(events.contains(&FileEvent::Started {
            declared_total: 4,
            file_type: 0x02
        }));
    }

    /// A `0x99` with no header cannot be checked and must not be delivered: there is no
    /// declared size, and the type byte would be whatever the PREVIOUS transfer left behind.
    #[test]
    fn an_end_frame_with_no_header_is_reported_not_delivered() {
        let mut r = FileReassembler::new();
        let events = r.push(&encode_file_frame(FileOpcode::End.code(), &[0x00]));
        assert_eq!(aborts(&events), vec![FileAbort::NoFileInfo]);
        assert!(completed(&events).is_none());
    }

    /// Data frames arriving with no open transfer are dropped rather than accumulated. Both
    /// reference implementations buffer them and reject at the end; this reaches the same
    /// outcome without letting a wire-supplied `addr` size an allocation.
    #[test]
    fn orphan_data_frames_allocate_nothing() {
        let mut r = FileReassembler::new();
        let mut p = 0x00FF_0000u32.to_be_bytes().to_vec();
        p.extend_from_slice(&[0xAB; 32]);
        let events = r.push(&encode_file_frame(FileOpcode::Data.code(), &p));
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(r.progress(), TransferProgress::default());
    }

    /// A declared size past the ceiling never opens a transfer, so nothing downstream can be
    /// driven by it. Neither reference implementation refuses here: iOS clamps the capacity hint and range-checks
    /// per frame, Android does neither.
    #[test]
    fn an_implausible_declared_size_never_opens_a_transfer() {
        for total in [0u32, (MAX_FILE_BYTES + 1) as u32, u32::MAX] {
            let mut r = FileReassembler::new();
            let mut p = total.to_be_bytes().to_vec();
            p.push(0x02);
            let events = r.push(&encode_file_frame(FileOpcode::Info.code(), &p));
            assert_eq!(
                aborts(&events),
                vec![FileAbort::ImplausibleSize {
                    declared_total: total as usize
                }]
            );
            assert!(!r.progress().is_active());

            // A data frame behind it therefore has nothing to grow.
            let mut d = 0u32.to_be_bytes().to_vec();
            d.extend_from_slice(&[0x01; 16]);
            assert!(r
                .push(&encode_file_frame(FileOpcode::Data.code(), &d))
                .is_empty());
        }
    }

    /// An address outside the declared file closes the transfer — but must NOT discard wire
    /// bytes that were already correctly framed. The reference iOS implementation calls its shared `reset()`
    /// here, which empties the framing buffer too.
    #[test]
    fn an_out_of_range_address_closes_the_transfer_without_losing_buffered_bytes() {
        let mut r = FileReassembler::new();
        r.push(&encode_file_frame(
            FileOpcode::Info.code(),
            &[0, 0, 0, 100, 0x02],
        ));

        let mut bad = 90u32.to_be_bytes().to_vec();
        bad.extend_from_slice(&[0xAA; 32]); // 90 + 32 = 122 > 100
                                            // Coalesce the offending frame with the start of the NEXT header, so a reset that
                                            // empties the byte buffer would eat it.
        let mut stream = encode_file_frame(FileOpcode::Data.code(), &bad);
        let next = encode_file_frame(FileOpcode::Info.code(), &[0, 0, 0, 4, 0x02]);
        stream.extend_from_slice(&next[..3]);

        let events = r.push(&stream);
        assert_eq!(
            aborts(&events),
            vec![FileAbort::OutOfRange {
                addr: 90,
                end: 122,
                declared_total: 100
            }]
        );
        assert_eq!(
            r.buffered(),
            &next[..3],
            "the next frame's prefix survives the abort"
        );
        let events = r.push(&next[3..]);
        assert!(events.contains(&FileEvent::Started {
            declared_total: 4,
            file_type: 0x02
        }));
    }

    /// `addr` spans the full u32 range and `usize` is exactly that wide on 32-bit Android, so
    /// the end offset is computed with `checked_add`. Without it the wrap makes a hostile frame
    /// look in-range.
    #[test]
    fn an_address_that_wraps_the_end_offset_is_rejected() {
        let mut r = FileReassembler::new();
        r.push(&encode_file_frame(
            FileOpcode::Info.code(),
            &[0, 0, 0, 100, 0x02],
        ));
        let mut p = u32::MAX.to_be_bytes().to_vec();
        p.extend_from_slice(&[0xAA; 32]);
        let events = r.push(&encode_file_frame(FileOpcode::Data.code(), &p));
        assert!(
            matches!(aborts(&events).as_slice(), [FileAbort::OutOfRange { .. }]),
            "{events:?}"
        );
    }

    /// A second `0x97` mid-transfer restarts the device's side. Both reference implementations drop the partial
    /// file silently; here it is reported, because a caller waiting on a photo needs to know
    /// the one it was waiting for is gone.
    #[test]
    fn a_second_header_supersedes_the_transfer_in_flight() {
        let (_, frames) = transfer(2_000, 0x02);
        let mut r = FileReassembler::new();
        let mut events = Vec::new();
        for f in &frames[..3] {
            events.extend(r.push(f));
        }
        events.extend(r.push(&frames[0]));
        assert_eq!(
            aborts(&events),
            vec![FileAbort::Superseded {
                received: 2 * CAPTURED_CHUNK_BYTES,
                declared_total: 2_000
            }]
        );
        assert_eq!(r.progress().received, 0, "the new transfer starts empty");
    }

    /// Progress is a query, not an event: a `0x98` lands ~27 times a second and the shell
    /// coalesces on a timer this module does not have.
    #[test]
    fn progress_tracks_genuinely_delivered_bytes_only() {
        let (_, frames) = transfer(2_000, 0x02);
        let mut r = FileReassembler::new();
        assert_eq!(r.progress(), TransferProgress::default());
        r.push(&frames[0]);
        assert_eq!(
            r.progress(),
            TransferProgress {
                received: 0,
                declared_total: 2_000,
                file_type: 0x02,
                first_gap: Some(0)
            }
        );
        // Skip the first chunk, deliver the second: 496 bytes present, but the file now spans
        // 992 bytes of buffer. Progress must report the former.
        r.push(&frames[2]);
        let p = r.progress();
        assert_eq!((p.received, p.first_gap), (CAPTURED_CHUNK_BYTES, Some(0)));
        assert!(p.is_active());
        r.reset();
        assert_eq!(r.progress(), TransferProgress::default());
    }

    /// The interval set the completeness check rests on.
    #[test]
    fn received_ranges_merge_overlaps_and_find_the_first_hole() {
        let mut s = ReceivedRanges::default();
        assert!(s.insert(10, 20));
        assert!(s.insert(0, 5));
        assert_eq!(s.covered(), 15);
        assert_eq!(s.first_gap(30), Some(5));
        assert!(s.insert(5, 10)); // abuts both neighbours — must fuse into one span
        assert_eq!(s.spans, vec![(0, 20)]);
        assert_eq!(s.covered(), 20);
        assert_eq!(s.first_gap(20), None);
        assert_eq!(s.first_gap(30), Some(20));
        assert!(s.insert(15, 18)); // wholly contained — no change
        assert_eq!(s.spans, vec![(0, 20)]);
        assert!(s.insert(7, 7)); // empty — ignored
        assert_eq!(s.spans, vec![(0, 20)]);
        assert!(s.insert(25, 30));
        assert!(s.insert(18, 27)); // bridges the two, absorbing both
        assert_eq!(s.spans, vec![(0, 30)]);
        assert_eq!(ReceivedRanges::default().first_gap(0), None);
    }

    /// The span count is driven straight off the wire, so it is capped. A stream that delivers
    /// every other chunk makes one span per chunk; past the cap the transfer is abandoned
    /// rather than tracked, and a span that FUSES with a neighbour never counts against it.
    #[test]
    fn the_received_span_count_is_capped() {
        let mut s = ReceivedRanges::default();
        for i in 0..MAX_RECEIVED_SPANS {
            assert!(
                s.insert(i * 4, i * 4 + 2),
                "span {i} is still inside the cap"
            );
        }
        assert_eq!(s.spans.len(), MAX_RECEIVED_SPANS);
        // A new hole is refused...
        assert!(!s.insert(MAX_RECEIVED_SPANS * 4, MAX_RECEIVED_SPANS * 4 + 2));
        assert_eq!(
            s.spans.len(),
            MAX_RECEIVED_SPANS,
            "and nothing was recorded"
        );
    }

    /// End to end: the cap ends the transfer with the outcome the `0x99` would have reported,
    /// instead of letting a fragmented stream grow the span list without bound.
    #[test]
    fn a_pathologically_fragmented_transfer_is_abandoned() {
        let total = (MAX_RECEIVED_SPANS + 2) * 4;
        let mut r = FileReassembler::default();
        let mut info = (total as u32).to_be_bytes().to_vec();
        info.push(0x02);
        r.push(&encode_file_frame(FileOpcode::Info.code(), &info));

        let mut abort = None;
        for i in 0..=MAX_RECEIVED_SPANS {
            // Two bytes every four: every chunk opens a new hole and never fuses.
            let mut payload = ((i * 4) as u32).to_be_bytes().to_vec();
            payload.extend_from_slice(&[0xAA, 0xBB]);
            let events = r.push(&encode_file_frame(FileOpcode::Data.code(), &payload));
            if let Some(a) = aborts(&events).first() {
                abort = Some(*a);
                break;
            }
        }
        assert!(
            matches!(
                abort,
                Some(FileAbort::Incomplete { declared_total, first_gap: Some(2), .. })
                    if declared_total == total
            ),
            "{abort:?}"
        );
        // The transfer is closed, so nothing is still being tracked.
        assert!(!r.progress().is_active());
    }

    /// The type byte is carried raw and the named values state their own evidence, so a caller
    /// branching on a thumbnail type can see it is branching on something no device has sent.
    #[test]
    fn only_the_hd_image_type_is_capture_backed() {
        assert_eq!(FileKind::HdImage.evidence(), Evidence::Capture);
        assert_eq!(FileKind::ImageThumb.evidence(), Evidence::ClientOnly);
        assert_eq!(FileKind::VideoThumb.evidence(), Evidence::ClientOnly);
        assert_eq!(FileKind::from_code(0x02), Some(FileKind::HdImage));
        assert_eq!(FileKind::from_code(0x00), None);
        for (h, _, kind, _) in GOLDEN_INFO {
            assert_eq!(FileKind::from_code(*kind), Some(FileKind::HdImage), "{h}");
        }
    }

    /// The cmd bytes are shared with the voice channel and only the envelope tells them apart.
    /// Stated as a test so that a future router keyed on the byte alone fails here.
    #[test]
    fn the_file_opcodes_collide_with_the_voice_opcodes_by_value() {
        use crate::DeviceUpload;
        assert_eq!(
            FileOpcode::Info.code(),
            DeviceUpload::VoiceUploadStart.code()
        );
        assert_eq!(FileOpcode::End.code(), DeviceUpload::VoiceUploadEnd.code());
        assert_eq!(FileOpcode::from_code(0x98), Some(FileOpcode::Data));
        assert_eq!(
            FileOpcode::from_code(0x46),
            None,
            "voice PCM is not a file frame"
        );
        // The envelopes do not accept each other's bytes.
        let file = hex(GOLDEN_END);
        assert_eq!(
            crate::decode_device(&file),
            Err(crate::DecodeError::BadHeader)
        );
        assert_eq!(
            decode_file_frame(&hex("ac55002a464b4105")),
            Err(FileFrameError::BadHeader)
        );
    }

    #[test]
    fn short_input_is_too_short_rather_than_a_header_or_checksum_error() {
        assert_eq!(decode_file_frame(&[]), Err(FileFrameError::TooShort));
        assert_eq!(
            decode_file_frame(&[0x52, 0x58, 0x00, 0x03]),
            Err(FileFrameError::TooShort)
        );
        // Header and length present, body short: waiting can fix this.
        assert_eq!(
            decode_file_frame(&hex("5258000797000029")),
            Err(FileFrameError::Truncated)
        );
    }

    /// Arbitrary payload sizes round-trip. Not a substitute for the golden vectors — a
    /// self-consistent codec can round-trip perfectly and still be wrong on the wire.
    #[test]
    fn encode_decode_round_trips_across_payload_sizes() {
        for n in 0..=CAPTURED_CHUNK_BYTES + 8 {
            let data: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_mul(37)).collect();
            let bytes = encode_file_frame(0x98, &data);
            assert_eq!(bytes.len(), FILE_FRAME_OVERHEAD + 1 + n + 1);
            assert_eq!(decode_file_frame(&bytes), Ok(Frame::new(0x98, data)));
        }
    }
}
