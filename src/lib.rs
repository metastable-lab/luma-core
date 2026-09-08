//! The Luma smart-glasses wire protocol — sans-IO, in one crate.
//!
//! Verified against an `E09` unit, project `T1` / customer `0303`, firmware bt **1.4.8** /
//! isp **1.3.1** / hw **2**. The written reference is `PROTOCOL.md`; this crate is its
//! executable half, and the integration guide in `docs/GUIDE.md` shows the two side by side.
//!
//! ## Scope — decisions and arithmetic, with the I/O behind feature flags
//!
//! The DEFAULT build owns the byte layer and nothing else: the opcode vocabulary, the frame
//! envelope, the checksum, the streaming demuxer, the Wi-Fi file API's URLs and reply parsers,
//! the RTSP message layer, and the RTP/H.264/AAC depacketisers. All of that is pure builders
//! and parsers with **zero dependencies** — `cargo tree` prints one line. It opens no socket,
//! holds no radio and reads no clock.
//!
//! The delivery is optional and lives in [`client`], each part behind a feature that names it:
//!
//! | feature | what it adds | dependencies |
//! |---|---|---|
//! | *(none)* | the protocol. Every module below except `client` and `ffi`. | none |
//! | `uniffi` | [`ffi`] — the Swift / Kotlin / Python surface | `uniffi` |
//! | `wifi-client` | [`client::fileapi`] — list / download / delete over the SoftAP | `ureq` |
//! | `ble` | [`client::ble`] — a BLE central: connect, handshake, photo, voice, Wi-Fi | `btleplug`, `tokio`, `futures` |
//! | `opus` | [`client::opus`] — wake-word packets to PCM, and a WAV writer | `opus` (system libopus) |
//! | `bindgen` | the bindings generator binary | `uniffi/cli` |
//!
//! That split is the point: an app embeds the default build and keeps its own radio, its own
//! HTTP and its own concurrency; a laptop turns two features on and drives a pair of glasses end
//! to end. Nothing in `client` re-derives a protocol fact — it is a thin driver over the modules
//! above it, which is what keeps the two halves from drifting.
//!
//! **Durations are NAMED here and measured by the caller.** [`timing`] holds them as
//! `std::time::Duration` constants — the ~2 s wait after an SSID arrives, the 1.2 s voice-idle
//! gap, the 30 s open-mic cap. A `Duration` is a quantity, not a clock: there is no `Instant` in
//! the default build, nothing sleeps, and nothing can tell you what time it is. The client still
//! owns every timer; it no longer has to rediscover what to set it to, which it previously did
//! one failed pairing at a time.
//!
//! ## Evidence — what is confirmed and what is not
//!
//! | Module | Evidence |
//! |---|---|
//! | [`frame`] | Verified on hardware. The envelope is pinned by golden vectors cut from six live sessions. |
//! | [`opcodes`] | Per-opcode. 39 of 53 declared names are verified in live sessions, 2 are device-probe results, 12 are defined but unexercised — [`Evidence`] states which, and a test forbids rounding one upward. |
//!
//! Three claims are worth calling out because they contradict something a reader will find in
//! a first reading of the command table:
//!
//! * **The `0x48` switch-state read has no `0x48` reply.** The firmware answers with a BURST
//!   of ten frames, each keyed by that setting's own SETTER opcode. Waiting for a `0x48`
//!   frame waits forever. Confirmed identically in six captures.
//! * **`0x99` (voice-upload-end) is never sent.** Four captures totalling 14,636 `0x46` voice
//!   frames contain zero. The mic closes when the app writes `0x56` and at no other time.
//! * **Frame fragmentation across notifications is INFERRED, not observed.** All eight
//!   captures were scanned: 15,085 `AC 55` frames, every one delivered whole and alone. The
//!   deframer is retained because it is strictly more general, not because a capture demands
//!   it — see [`frame`] for the full note.
//!
//! The gesture slot→gesture MAPPING is likewise inferred: the ids and their values are on the
//! wire, but no capture has ever changed a binding, so which slot is "double tap" rests on the
//! captured defaults matching the PDF's default table. [`GestureSlot`] says so in its own
//! docs, and settling it needs one vendor-app interaction, not more code.
//!
//! ## Conventions
//!
//! **Byte order.** Every multi-byte field on this wire is BIG-endian — the frame length, the
//! `0x02` record duration, the `0x42` thumbnail count, the `0x95` capability word. There is no
//! little-endian field in the protocol.
//!
//! **Numbers are not consistently encoded, and the inconsistency is the wire's, not ours.**
//! `0x17`'s battery reply carries ASCII digits (and overflows `'9'` to `0x3A` at 100 %), while
//! `0x53`'s battery PUSH carries a raw byte for the same quantity. Settings values are ASCII
//! digits except `0x02`, which is a 2-byte integer. Decoders here follow the field, never a
//! family rule.
//!
//! **Time.** `0x59` pushes the phone's wall clock as `YY MM DD HH MM SS` in **plain hex, not
//! BCD** (`1a 07 1b 12 28 12` = 2026-07-27 18:40:18) — the PDF's "BCD" wording is wrong and a
//! BCD encoder sets the device clock to a garbage date. This crate builds that payload from
//! fields the caller supplies and never reads a clock. Nothing else in this protocol carries a
//! timestamp: the glasses date their own media in the filename
//! (`EVENT/20260727225147720.jpg`) over the Wi-Fi file API, which is the client's.
//!
//! **Units.** No physical units cross this wire. Battery is a percentage, `0x02` is seconds,
//! volume levels are an unpinned raw scale (observed `0x05`…`0x10`) carried verbatim rather
//! than normalised — normalising an unpinned scale is how a slider ends up lying.
//!
//! ## GATT (the client's, recorded here so every client agrees)
//!
//! | | |
//! |---|---|
//! | service | `AA12` |
//! | write, app → device | `AA13`, Write **Request** (with response) |
//! | control notify | `AA14` |
//! | file/voice notify | `AA15` |
//!
//! `AE00`/`AE01`/`AE02` and the standard Battery service are present but unused by this protocol; no client
//! touches them. On this firmware voice PCM and control replies BOTH arrive on `AA14`; `AA15`
//! carries the `52 58` file stream — a second, unrelated framing (`52 58 | len(2 BE) | cmd |
//! payload | crc | 58 52`) that belongs to [`reassembly`] and is deliberately not handled by
//! [`frame`].
//!
//! That table is also DATA, in [`gatt`] — including the one fact that is easy to get wrong:
//! `AA13` takes an ATT Write **Request**, which iOS derives correctly from the
//! characteristic's properties and an Android client must be told explicitly.
//!
//! ## The modules
//!
//! | § of `PROTOCOL.md` | module | what it holds |
//! |---|---|---|
//! | §1 transport | [`gatt`] | the service and characteristic UUIDs, and the write mode, as data |
//! | §2 frame format | [`frame`] | the `AB 55` / `AC 55` envelope, the checksum, the streaming deframer |
//! | §4 command table | [`opcodes`] | every opcode with its [`Evidence`] level |
//! | §4 builders | [`commands`] | one function per row: `take_photo`, `set_volume`, … |
//! | §5–§11 replies | [`parser`] | `AA14` bytes → [`parser::DeviceEvent`], and the settings accumulator |
//! | §10 voice | [`voice`] | the wake-word stream state machine and the Opus packet header |
//! | §13 file API | [`fileapi`] | the URLs, the JSON replies, the KiB completeness rule, the sync order |
//! | §14 live view | [`rtsp`], [`sdp`], [`rtp`], [`h264`], [`aac`] | the RTSP conversation, the description, RTP, and the two depacketisers |
//! | §15 file stream | [`reassembly`] | the `52 58` framing on `AA15` — the AI image |
//! | §3, §10, §12 | [`timing`] | every recommended duration, named |
//! | the I/O | [`client`] | sockets, a radio and a codec, behind feature flags |
//! | other languages | [`ffi`] | the UniFFI boundary (feature `uniffi`) |
//!
//! ## Reaching this from another language
//!
//! [`ffi`] (behind the `uniffi` feature) is the UniFFI boundary: objects for the stateful
//! pieces (`GlassesParser`, `GlassesFileReassembler`, `GlassesVoiceStream`,
//! `GlassesSwitchStates`, `GlassesDeframer`, `GlassesRtspSession`, `GlassesH264Depacketizer`,
//! `GlassesAacDepacketizer`) and `glasses_*` free functions for the codec, the opcode tables,
//! the GATT topology, the [`commands`] builders, the Wi-Fi file API's URLs and parsers, the SDP
//! parser, and the [`timing`] constants as `glasses_timing_*_ms()`.
//!
//! Nothing there is async, and nothing there measures a duration — the constants cross as
//! milliseconds and the client owns the timer.

pub mod aac;
pub mod client;
pub mod commands;
#[cfg(feature = "uniffi")]
pub mod ffi;
pub mod fileapi;
pub mod frame;
pub mod gatt;
pub mod h264;
pub mod opcodes;
pub mod parser;
pub mod reassembly;
pub mod rtp;
pub mod rtsp;
pub mod sdp;
pub mod timing;
pub mod voice;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use aac::{AacError, AuHeader, AuHeaderConfig, AudioSpecificConfig};
pub use commands::LedLevel;
pub use fileapi::{
    download_is_complete, expected_byte_range, DeleteReply, FileEntry, FileList, Folder,
    FolderListing, MediaKind, SyncPlan, SyncStep, Timestamp, HOST as WIFI_HOST,
    PASSPHRASE as WIFI_PASSPHRASE, PHONE_ADDRESS as WIFI_PHONE_ADDRESS,
};
pub use frame::{
    checksum, decode_app, decode_device, drain, encode, encode_command, encode_device,
    is_partial_frame_prefix, DecodeError, Deframer, Frame, Residual, APP_HEADER, DEVICE_HEADER,
    FRAME_OVERHEAD, MAX_LENGTH_FIELD, MIN_LENGTH_FIELD,
};
pub use h264::{AccessUnit, DepacketizerStats, NAL_IDR, NAL_PPS, NAL_SPS, START_CODE};
pub use opcodes::{
    AppCommand, DeviceUpload, Evidence, GestureAction, GestureSlot, VolumeChannel,
    SWITCH_STATE_BURST_ORDER, VENDOR_NAMED_UNIMPLEMENTED,
};
pub use reassembly::{
    decode_file_frame, drain_file_frames, encode_file_frame, FileAbort, FileEvent, FileFrameError,
    FileKind, FileOpcode, FileReassembler, ReassembledFile, TransferProgress, CAPTURED_CHUNK_BYTES,
    FILE_FRAME_OVERHEAD, FILE_HEADER, FILE_TRAILER, MAX_FILE_BYTES,
};
pub use rtp::{RtpError, RtpHeader, RtpPacket, SequenceReport, SequenceTracker};
pub use rtsp::{RtspError, RtspSession, Track, Transport};
pub use sdp::{Fmtp, MediaDescription, RtpMap, SdpError, SessionDescription};
pub use timing::{
    BATTERY_POLL_MAX, BATTERY_POLL_MIN, CAPABILITIES_PUSH_DELAY, FILE_STREAM_STALL,
    PHOTO_CAPTURE_TYPICAL, SSID_ARRIVAL_TYPICAL, SSID_SETTLE, STATE_REFRESH, VOICE_HARD_CAP,
    VOICE_IDLE_GAP, VOICE_INTERRUPT_REPEAT_GAP, WIFI_JOIN_TIMEOUT,
};
pub use voice::{
    EndCause, OpusBandwidth, OpusMode, OpusToc, VoiceEvent, VoicePacket, VoiceStream,
    BITS_PER_SAMPLE, CAPTURED_PACKET_BYTES, CHANNELS, REQUESTED_SAMPLE_RATE_HZ,
};
