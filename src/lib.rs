//! The smart-glasses BLE wire protocol — sans-IO, in one crate.
//!
//! Hardware under test: an `E09` unit, project `T1` / customer `0303`, bt **V1.4.8** / isp
//! **V1.3.1** / hw **V2**. Two vendor apps drive the same firmware and were captured side by
//! side — **EyeVue** 3.2.7 and **GlassX** 3.0.8, both skins on the Watchfun platform — which
//! is why they share opcodes and differ in which ones they bother to send. The reverse-
//! engineering write-up is `PROTOCOL.md`; this crate is its executable half.
//!
//! ## Scope — decisions and arithmetic only
//!
//! This crate owns the byte layer: the opcode vocabulary, the frame envelope, the checksum,
//! and the streaming demuxer. It owns none of the delivery: `CBCentralManager` /
//! `BluetoothGatt`, the SoftAP join, the RTSP client, the AAC/H.264 depacketisers and every
//! socket stay in the client, because that is where the platforms genuinely differ.
//!
//! Timing DURATIONS are client policy and are absent on purpose. The protocol has several —
//! the ~2 s wait after an SSID arrives before joining the AP, the 1.2 s voice-idle gap, the
//! 30 s open-mic cap — and every one of them is a number the client chooses. This crate may
//! NAME a pause; it may not measure one, and it has no clock with which to try.
//!
//! ## Evidence — what is confirmed and what is not
//!
//! | Module | Evidence |
//! |---|---|
//! | [`frame`] | DEVICE-CONFIRMED. The envelope is re-derived from packet captures and pinned by golden vectors cut from six of them, not from any vendor source. |
//! | [`opcodes`] | Per-opcode. 39 of 53 declared names are CAPTURE-confirmed, 2 are DEVICE-PROBE results, 12 are CLIENT-ONLY — [`Evidence`] states which, and a test forbids rounding one upward. |
//!
//! Three claims are worth calling out because they contradict something a reader will find in
//! a vendor app or the vendor PDF:
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
//! `AE00`/`AE01`/`AE02` and the standard Battery service are present but neither vendor app
//! touches them. On this firmware voice PCM and control replies BOTH arrive on `AA14`; `AA15`
//! carries the `52 58` file stream — a second, unrelated framing (`52 58 | len(2 BE) | cmd |
//! payload | crc | 58 52`) that belongs to [`reassembly`] and is deliberately not handled by
//! [`frame`].
//!
//! That table is also DATA, in [`gatt`] — including the one fact that is easy to get wrong:
//! `AA13` takes an ATT Write **Request**, which iOS derives correctly from the
//! characteristic's properties and an Android client must be told explicitly.
//!
//! ## Reaching this from another language
//!
//! [`ffi`] (behind the `uniffi` feature) is the UniFFI boundary: objects for the stateful
//! pieces (`GlassesParser`, `GlassesFileReassembler`, `GlassesVoiceStream`,
//! `GlassesSwitchStates`, `GlassesDeframer`) and `glasses_*` free functions for the codec, the
//! opcode tables, the GATT topology and the [`commands`] builders.
//!
//! Nothing there is async, and nothing there measures a duration: the 1.2 s voice-idle gap, the
//! 30 s open-mic cap and the ~2 s SSID wait are all the client's.

pub mod commands;
#[cfg(feature = "uniffi")]
pub mod ffi;
pub mod frame;
pub mod gatt;
pub mod opcodes;
pub mod parser;
pub mod reassembly;
pub mod voice;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use commands::LedLevel;
pub use frame::{
    checksum, decode_app, decode_device, drain, encode, encode_command, encode_device,
    is_partial_frame_prefix, DecodeError, Deframer, Frame, Residual, APP_HEADER, DEVICE_HEADER,
    FRAME_OVERHEAD, MAX_LENGTH_FIELD, MIN_LENGTH_FIELD,
};
pub use opcodes::{
    AppCommand, DeviceUpload, Evidence, GestureAction, GestureSlot, VolumeChannel,
    SWITCH_STATE_BURST_ORDER, VENDOR_NAMED_UNIMPLEMENTED,
};
pub use reassembly::{
    decode_file_frame, drain_file_frames, encode_file_frame, FileAbort, FileEvent, FileFrameError,
    FileKind, FileOpcode, FileReassembler, ReassembledFile, TransferProgress, CAPTURED_CHUNK_BYTES,
    FILE_FRAME_OVERHEAD, FILE_HEADER, FILE_TRAILER, MAX_FILE_BYTES,
};
pub use voice::{
    EndCause, OpusBandwidth, OpusMode, OpusToc, VoiceEvent, VoicePacket, VoiceStream,
    BITS_PER_SAMPLE, CAPTURED_PACKET_BYTES, CHANNELS, REQUESTED_SAMPLE_RATE_HZ,
};
