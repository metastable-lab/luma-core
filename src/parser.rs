//! Inbound decode: one `AC 55` control frame → one typed [`DeviceEvent`].
//!
//! Port of the reference iOS client's packet parser (370 lines of Swift) and the field
//! decoders it feeds, cross-checked against its Android counterpart (185 lines of Kotlin).
//!
//! ## Why this is one decode and not a demuxer
//!
//! Neither reference implementation has a "parser" in the sense this file is one. Both are
//! channel demuxers and thread-hop reducers: they drain frames, fold voice PCM, and forward
//! everything else as an opaque upload packet for the UI-thread manager to pick apart with
//! byte indices 800 lines away. **Every field offset in the reference iOS app is written at a
//! `@Published` assignment**, which is why its battery decode carries a comment about the
//! readout freezing at 100 % — the bug lived in the UI layer because the decode did.
//!
//! Here the split is: [`frame`](crate::frame) owns the envelope, this module owns the
//! FIELDS, and the shell owns the radio, the threads and the codecs. A shell never sees a
//! byte offset. What is deliberately NOT here: the `52 58` file reassembly
//! ([`crate::reassembly`] — a different framing on a different characteristic), the
//! `0x97`/`0x46`/`0x99` capture machine and the Opus header ([`crate::voice`], which
//! [`Parser`] drives rather than reimplements), the Opus DECODE (a platform codec —
//! CoreAudio on iOS, `MediaCodec` on Android), the queue hopping, and every timer. Timing
//! DURATIONS are shell policy by design: this module can say a voice capture is OPEN, it
//! cannot say for how long, and the 1.2 s idle gap and 30 s open-mic cap are numbers the
//! shell owns.
//!
//! ## Traps this decode is built to avoid
//!
//! Each of these was a live bug in a shipping client, and each is a fact about the firmware
//! rather than about that client:
//!
//! | frame | the trap | what the firmware actually does |
//! |---|---|---|
//! | `0x17` battery reply | routed by a `kind` that is null for `0x17`, so the reply falls through an `else` and is dropped | a client polling `0x17` as a keepalive discards every reply and the battery then moves only on the `0x53` push — ~10 s discharging, ~60 s charging, and never at connect |
//! | the `0x48` burst — `0x01`, `0x02`, `0x04`, `0x06`, `0x61` and all five gestures | waiting for an aggregate `AC 55 … 48 …` frame | that frame does not exist on this firmware (six captures). The branch cannot fire, ten real frames are dropped, and the settings model never leaves its defaults |
//! | `0x45` index 9 (`importingMode`) | reading nine flags because the PDF says nine | the device sends TEN; a Wi-Fi session's import mode is otherwise invisible |
//! | `0x45` index 0 (`takePhoto`) | named in a comment, never assigned | photo-in-progress is otherwise invisible |
//! | `0x45` indices 5/6 | bound to head-nod / head-shake gestures | those bits have NEVER fired in any capture |
//!
//! One total decode makes all of that impossible: [`parse`] is total, so a byte either has a
//! typed meaning every client gets, or is reported as [`DeviceEvent::Unrecognised`]. Nothing
//! can be dropped on one platform and handled on another.
//!
//! ## Evidence
//!
//! Every field offset below was re-read off the packet captures rather than trusted from
//! either reference implementation. The census behind the claims in this file, over eight
//! captures and 15,403 ATT values:
//!
//! | opcode | inbound frames | distinct payloads | captures |
//! |---|---|---|---|
//! | `0x46` voice | 14,636 | — | 4 |
//! | `0x45` action sync | 85 | 13 | 8 |
//! | `0x17` battery read | 74 | 15 | 8 |
//! | `0x69` volumes | 55 | 20 | 7 |
//! | `0x42` media count | 25 | 13 | 7 |
//! | `0x95` capabilities | 17 | **1** | 6 |
//! | `0x55` versions | 15 | **1** | 5 |
//! | `0x53` battery push | 11 | 11 | 3 |
//! | `0x64` identity | 10 | **1** | 5 |
//! | `0x25` SSID | 4 | **1** | 4 |
//! | `0x71` voice features | 1 | 1 | 1 |
//!
//! Three things that census settles, each of which contradicts something a reader will
//! find in the Swift, the Kotlin or the PDF:
//!
//! * **`0x17` and `0x53` are the same quantity in two encodings, and the captures prove
//!   it rather than merely allowing it.** In `eyevue_1` the ASCII read says 48/49/50/51 %
//!   charging while the raw push says 49/50/51; in `glassx_1` the read says 21…25 %
//!   discharging while the push says 20…24. Two independent encodings tracking one
//!   battery across three captures is why [`BatteryReading`] can carry both under one
//!   type instead of two hopeful structs.
//! * **The `0x25` SSID payload is NOT NUL-terminated on this firmware.** All four captures
//!   carry the same 13 bare ASCII bytes with no terminator and no padding, `len = 0x0F`.
//!   The "NUL-terminated" wording in `PROTOCOL.md` §3 and the Swift's
//!   NUL-stripping are defensive, not observed. The stripping is kept — it costs nothing
//!   and another SKU may pad — but it is labelled for what it is.
//! * **`0x40` never comes back.** It is written eight times across the captures and there
//!   is not one inbound `0x40` frame; the count arrives as `0x42`. The iOS manager's
//!   `default:` arm that decodes a `0x40` reply has therefore never fired on any device we
//!   hold, and [`CountSource::ReadReply`] is marked INFERRED so a caller can tell.
//!
//! ## Malformed is an event, not a silent drop
//!
//! Every decode that cannot complete produces [`DeviceEvent::Malformed`] carrying the raw
//! bytes, and no decode clamps. This is the house rule from
//! [`GestureAction::from_value`](crate::GestureAction::from_value) applied throughout, and
//! it is not academic: the iOS battery decode used `Int(String(...))`, which returned nil
//! for the `':'` that 100 % encodes as, and dropped **every** reading while the glasses sat
//! at 100 % — a frozen readout with nothing in the logs. A clamp would have been worse: it
//! would have shown a plausible wrong number.

use crate::frame::{Deframer, Frame, Residual};
use crate::opcodes::{
    AppCommand, DeviceUpload, Evidence, GestureAction, GestureSlot, VolumeChannel,
};
use crate::voice::{EndCause, VoiceEvent, VoiceStream};

/// The dispatch bytes, derived from the opcode enums rather than re-typed.
///
/// A `match` arm needs a constant pattern and `AppCommand::GetBattery.code()` is a function
/// call, so each byte is spelled `Variant as u8` here. That keeps this dispatch table tied
/// to [`opcodes`](crate::opcodes): renumber a variant there and the parser follows, rather
/// than quietly continuing to decode the old byte.
mod op {
    use super::{AppCommand, DeviceUpload};

    pub const LED: u8 = AppCommand::SetLed as u8;
    pub const RECORD_DURATION: u8 = AppCommand::SetRecordDuration as u8;
    pub const WEAR_DETECTION: u8 = AppCommand::WearDetection as u8;
    pub const VOICE_COMMAND: u8 = AppCommand::VoiceCommand as u8;
    pub const GESTURE_SWIPE_FORWARD: u8 = AppCommand::SetGestureSwipeForward as u8;
    pub const GESTURE_SWIPE_BACK: u8 = AppCommand::SetGestureSwipeBack as u8;
    pub const GESTURE_SINGLE_TAP: u8 = AppCommand::SetGestureSingleTap as u8;
    pub const GESTURE_DOUBLE_TAP: u8 = AppCommand::SetGestureDoubleTap as u8;
    pub const GESTURE_TRIPLE_TAP: u8 = AppCommand::SetGestureTripleTap as u8;
    pub const BATTERY_READ: u8 = AppCommand::GetBattery as u8;
    pub const OPEN_WIFI_FILES: u8 = AppCommand::OpenWifiFiles as u8;
    pub const FILE_COUNT_READ: u8 = AppCommand::GetFileCount as u8;
    pub const VERSIONS: u8 = AppCommand::GetVersions as u8;
    pub const ORIENTATION: u8 = AppCommand::Orientation as u8;
    pub const IDENTITY: u8 = AppCommand::GetProjectName as u8;
    pub const OPEN_WIFI_LIVE: u8 = AppCommand::OpenWifiLive as u8;
    pub const VOICE_FEATURES: u8 = AppCommand::GetVoiceDisableState as u8;
    pub const CAPABILITIES: u8 = AppCommand::GetCapabilities as u8;

    pub const WIFI_NAME: u8 = DeviceUpload::WifiName as u8;
    pub const MEDIA_COUNT: u8 = DeviceUpload::ThumbnailCount as u8;
    pub const ACTION_SYNC: u8 = DeviceUpload::ActionSync as u8;
    pub const VOICE_DATA: u8 = DeviceUpload::VoiceData as u8;
    pub const ABANDON_VOICE: u8 = DeviceUpload::AbandonVoice as u8;
    pub const CANCEL_AI_BROADCAST: u8 = DeviceUpload::CancelAiBroadcast as u8;
    pub const HD_IMAGE_FAILED: u8 = DeviceUpload::HdImageFailed as u8;
    pub const BATTERY_PUSH: u8 = DeviceUpload::ChargeBattery as u8;
    pub const VOLUMES: u8 = DeviceUpload::Volumes as u8;
    pub const ISP_UPGRADE_DONE: u8 = DeviceUpload::IspUpgradeDone as u8;
    pub const VOICE_START: u8 = DeviceUpload::VoiceUploadStart as u8;
    pub const VOICE_END: u8 = DeviceUpload::VoiceUploadEnd as u8;
}

// ---------------------------------------------------------------------------------------
// Field types
// ---------------------------------------------------------------------------------------

/// Which frame a battery reading came from. Same quantity, two encodings — see the module
/// docs for the cross-capture proof that they track one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BatterySource {
    /// `0x17` reply: `<tens><ones><charging>`, the two digits ASCII (`0x30 + n`).
    /// DEVICE-CONFIRMED in all eight captures.
    ReadReply,
    /// `0x53` unsolicited push: `<charging><percent>`, percent a RAW byte and the fields
    /// in the OPPOSITE order. DEVICE-CONFIRMED in `eyevue_1`, `eyevue_2`, `glassx_1`.
    Push,
}

/// A decoded battery state.
///
/// `percent` is `0..=100`. Anything outside that is [`DeviceEvent::Malformed`], never a
/// clamp: a clamp turns a corrupt frame into a plausible wrong number on the user's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8,
    pub charging: bool,
    pub source: BatterySource,
}

/// Firmware versions from `0x55`, three fields in one 7-byte payload.
///
/// DEVICE-CONFIRMED: `01 04 08 01 03 01 02` on every unit in five captures — bt V1.4.8,
/// isp V1.3.1, hw V2. One distinct payload across fifteen frames, which is what makes this
/// a fingerprint of the firmware under test rather than a sample of a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Versions {
    /// Bluetooth firmware, `[major, minor, patch]`.
    pub bt: [u8; 3],
    /// ISP (camera) firmware, `[major, minor, patch]`.
    pub isp: [u8; 3],
    /// Hardware revision, one byte.
    pub hardware: u8,
}

/// Project and customer codes from `0x64` — 8 bytes, 4 ASCII each, NUL-padded.
///
/// DEVICE-CONFIRMED as `54 31 00 00 30 33 30 33` = `"T1"` / `"0303"` on every unit. The
/// vendor's own logs call the concatenation `equipmentCode T10303`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub project: String,
    pub customer: String,
}

/// Where a media count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CountSource {
    /// `0x42`. DEVICE-CONFIRMED, 25 frames across seven captures.
    Push,
    /// `0x40` echoed back with a count.
    ///
    /// **INFERRED.** The iOS manager decodes this; no capture contains an inbound `0x40`
    /// frame, though the opcode is written eight times. The device answers `0x40` with a
    /// `0x42`, exactly as `PROTOCOL.md` §3 says. Kept because decoding a
    /// count is harmless and the branch costs nothing — but a shell that treats this as
    /// evidence the read-reply exists is wrong.
    ReadReply,
}

/// How many photos/videos the device is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaCount {
    pub count: u16,
    pub source: CountSource,
}

/// `0x95` device capability word — 4 bytes BIG-endian, deliberately NOT bit-decoded.
///
/// DEVICE-CONFIRMED as `00 00 04 0B` on every unit in six captures — one distinct payload
/// in seventeen frames. Four bits are set and the vendor app shows exactly four true flags,
/// but it prints its map alphabetically, so the bit↔flag assignment cannot be recovered
/// from a single value. Decoding it would be inventing a mapping; the honest type is the
/// raw word plus "is this the hardware we characterised".
///
/// The device also PUSHES this ~30 ms after every LE link-up, before the app writes
/// anything, so one can arrive with no request outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub raw: u32,
}

impl Capabilities {
    /// The word every captured unit of this model reports.
    pub const KNOWN_E09: u32 = 0x0000_040B;

    /// True when this device reports the capability word we have characterised. A
    /// different word is not an error — it means nobody has seen that configuration.
    pub fn is_known_configuration(self) -> bool {
        self.raw == Self::KNOWN_E09
    }
}

/// `0x71` — which voice features are currently DISABLED, as a 1-byte bitmap.
///
/// CAPTURE-CONFIRMED, but only ever as `0x00` (one frame, `eyevue_1`), so the BIT
/// POSITIONS are unknown and this type does not pretend otherwise. The vendor expands it to
/// `{ aiAwaken, offlineVoice }`; which bit is which needs a unit with a voice feature
/// switched off. [`Self::all_enabled`] is the one reading the evidence supports, and it is
/// the one that matters — it means the wake word is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceFeatures {
    pub raw: u8,
}

impl VoiceFeatures {
    /// True when the device reports nothing disabled.
    pub fn all_enabled(self) -> bool {
        self.raw == 0
    }
}

/// The three output levels from a `0x69` reply, in the capture-pinned order.
///
/// Levels are carried VERBATIM. The scale is not pinned — captured values span `0x06`…`0x10`
/// across seven captures with no maximum ever observed — and normalising an unpinned scale
/// is how a slider ends up lying about where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volumes {
    pub system: u8,
    pub media: u8,
    pub call: u8,
}

impl Volumes {
    /// The level for one channel. Stated as a function so [`VolumeChannel`]'s ordering and
    /// this struct's field order are checked against each other rather than assumed.
    pub fn level(self, channel: VolumeChannel) -> u8 {
        match channel {
            VolumeChannel::System => self.system,
            VolumeChannel::Media => self.media,
            VolumeChannel::Call => self.call,
        }
    }
}

/// The SoftAP the glasses just raised, from a `0x25` frame.
///
/// The passphrase is not on the wire: it is the fixed `12345678` on this firmware, from the
/// vendor logs and confirmed by every successful join in the Wi-Fi captures. It lives on this type
/// so the shell that joins the AP does not have to carry a magic string of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiCredentials {
    pub ssid: String,
}

impl WifiCredentials {
    /// The factory-fixed WPA passphrase. Same for `0x39` and `0x67` — the SoftAP is one AP
    /// serving two different things.
    pub const PASSPHRASE: &'static str = "12345678";

    pub fn passphrase(&self) -> &'static str {
        Self::PASSPHRASE
    }
}

/// Which of the two Wi-Fi modes an ack belongs to. Same SoftAP, same SSID, same `0x44`
/// teardown; the device serves something different behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WifiService {
    /// `0x39` — the JSON file API (`/app/getfilelist`). The gallery path.
    Files,
    /// `0x67` — the RTSP live stream (`rtsp://192.168.169.1:554/h264`).
    Live,
}

impl WifiService {
    /// The command that opens this mode.
    pub fn command(self) -> AppCommand {
        match self {
            WifiService::Files => AppCommand::OpenWifiFiles,
            WifiService::Live => AppCommand::OpenWifiLive,
        }
    }
}

/// One field of the `0x48` switch-state burst, or the echo of the setter that changed it.
///
/// The firmware never sends an aggregate `0x48` frame — see
/// [`SWITCH_STATE_BURST_ORDER`](crate::SWITCH_STATE_BURST_ORDER). Gesture slots are NOT in
/// here; they get their own event because they are the surface Android is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReport {
    /// `0x01` — indicator-LED brightness, `0..=2`. All three values captured (`'0'`, `'1'`,
    /// `'2'`), which is the one thing that says the PDF's "low/mid/high, no off" is a
    /// complete range rather than a partial observation. A dead feature — no client drives
    /// it — but the frame is real and arrives in every burst.
    Led(u8),
    /// `0x02` — video/loop record duration in seconds, 2-byte BIG-endian. The one burst
    /// value that is not an ASCII digit. Captured as 60, 90 and 180.
    RecordSeconds(u16),
    /// `0x04` — wear detection. Captured only as `'1'`.
    WearDetection(bool),
    /// `0x06` — voice command / wake word. Captured only as `'1'`.
    VoiceCommand(bool),
    /// `0x61` — capture orientation. Captured only as `'0'` (portrait).
    Orientation(Orientation),
}

/// `0x61` capture orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Orientation {
    /// Wire `'0'`.
    Portrait,
    /// Wire `'1'`.
    Landscape,
}

/// One action flag in the `0x45` action-sync frame.
///
/// The names are the vendor's own, from its `收到设备动作同步状态` dictionary. The PDF says
/// nine fields; **the device sends ten**, and index 9 is file-IMPORT mode rather than the
/// "Wi-Fi active" a reader would guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum DeviceAction {
    TakePhoto = 0,
    AudioRecord = 1,
    VideoRecord = 2,
    VolumeUp = 3,
    VolumeDown = 4,
    /// Nod. Never observed set in any capture.
    NodHead = 5,
    /// Shake. Never observed set in any capture.
    ShakeHead = 6,
    PlayPause = 7,
    /// Never observed set in any capture, despite `0x04` wear detection being reported ON
    /// by every unit — so "wear detection is enabled" and "the device reports wearing" are
    /// not the same claim, and only the first has evidence.
    WearingDetection = 8,
    /// Goes 1 while a Wi-Fi session is open. NOT "Wi-Fi active" — the vendor calls it
    /// file-import mode.
    ImportingMode = 9,
}

impl DeviceAction {
    pub const ALL: [DeviceAction; 10] = [
        DeviceAction::TakePhoto,
        DeviceAction::AudioRecord,
        DeviceAction::VideoRecord,
        DeviceAction::VolumeUp,
        DeviceAction::VolumeDown,
        DeviceAction::NodHead,
        DeviceAction::ShakeHead,
        DeviceAction::PlayPause,
        DeviceAction::WearingDetection,
        DeviceAction::ImportingMode,
    ];

    /// Byte offset within the payload.
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(index: usize) -> Option<DeviceAction> {
        DeviceAction::ALL.into_iter().find(|a| a.index() == index)
    }

    /// What backs this flag.
    ///
    /// Seven of the ten have been seen SET on a real device; three never have, across all
    /// 85 action-sync frames in the eight captures. That is a fact about the flags, not
    /// about the frame — the frame itself is [`Evidence::Capture`] and always ten bytes
    /// long. A UI binding built on a [`Evidence::ClientOnly`] flag renders a state the
    /// hardware has never reported, which is exactly why the reference client dropped the head-gesture
    /// bindings.
    pub fn evidence(self) -> Evidence {
        use DeviceAction::*;
        match self {
            TakePhoto | AudioRecord | VideoRecord | VolumeUp | VolumeDown | PlayPause
            | ImportingMode => Evidence::Capture,
            NodHead | ShakeHead | WearingDetection => Evidence::ClientOnly,
        }
    }
}

/// The `0x45` action-sync snapshot — ten independent flags in one frame.
///
/// Stored as a bitmap rather than ten bools so the whole state compares in one instruction;
/// the accessor is what callers use, and no shell sees an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActionSync {
    flags: u16,
}

impl ActionSync {
    /// Build from the ten payload bytes.
    ///
    /// A byte is active when it is NON-ZERO. Both reference implementations test `== 0x01`; every one of the
    /// 850 captured flag bytes is `0x00` or `0x01`, so the two readings agree on all
    /// evidence we hold, and `!= 0` cannot produce a false NEGATIVE on an encoding we have
    /// not seen. Where they could differ, this is the direction that fails safe.
    pub fn from_flags(bytes: &[u8; 10]) -> ActionSync {
        let mut flags = 0u16;
        for (i, b) in bytes.iter().enumerate() {
            if *b != 0 {
                flags |= 1 << i;
            }
        }
        ActionSync { flags }
    }

    pub fn is_active(self, action: DeviceAction) -> bool {
        self.flags & (1 << action.index()) != 0
    }

    /// Every action currently reported active, in index order.
    pub fn active(self) -> Vec<DeviceAction> {
        DeviceAction::ALL
            .into_iter()
            .filter(|a| self.is_active(*a))
            .collect()
    }

    /// The bitmap, bit *i* = payload index *i*. For logging and equality only.
    pub fn raw(self) -> u16 {
        self.flags
    }
}

/// Why a frame could not be decoded into its typed field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedReason {
    /// Payload shorter than the field layout needs. `need` is the minimum length.
    TooShort { need: usize },
    /// The layout is satisfied but a value is outside the range it can legally take — a
    /// battery over 100 %, an LED level above 2, a boolean setting that is neither 0 nor 1.
    OutOfRange,
    /// A text field is not ASCII, or is empty once NUL padding and control bytes are gone.
    NotText,
}

// ---------------------------------------------------------------------------------------
// The event
// ---------------------------------------------------------------------------------------

/// One decoded inbound frame.
///
/// Total: [`parse`] returns a variant for every possible `cmd` byte, so there is no silent
/// drop anywhere in the decode. Frames we recognise but that carry no state land in
/// [`Self::Ack`]; bytes in neither opcode table land in [`Self::Unrecognised`] with their
/// payload intact, which is how the `0x36` probe was characterised without first enshrining
/// the opcode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// `0x17` or `0x53`. The source says which encoding it arrived in.
    Battery(BatteryReading),
    /// `0x55`.
    Versions(Versions),
    /// `0x64`.
    Identity(Identity),
    /// `0x42` (confirmed) or `0x40` (inferred).
    MediaCount(MediaCount),
    /// `0x01` / `0x02` / `0x04` / `0x06` / `0x61` — one field of the `0x48` burst, or the
    /// echo of the setter that changed it. The two are indistinguishable on the wire and
    /// mean the same thing, so they are one event.
    Switch(SwitchReport),
    /// `0x07`–`0x11` — a touchpad gesture binding.
    ///
    /// The whole touch-input surface. Neither reference implementation decoded these, so
    /// they arrive here as a typed event for the first time. `action` is `None` when the
    /// value byte is outside the known set —
    /// `raw` is always the byte the device sent, so an unmapped binding is visible rather
    /// than reported as "volume down".
    ///
    /// Slot→gesture MAPPING is INFERRED (see [`GestureSlot`]); the slot IDS and their
    /// captured values are DEVICE-CONFIRMED on six units.
    GestureBinding {
        slot: GestureSlot,
        action: Option<GestureAction>,
        raw: u8,
    },
    /// `0x95`.
    Capabilities(Capabilities),
    /// `0x71`.
    VoiceFeatures(VoiceFeatures),
    /// `0x69`.
    Volumes(Volumes),
    /// `0x25` — the SoftAP is up and this is how to join it.
    WifiCredentials(WifiCredentials),
    /// `0x39` / `0x67` ack: the device accepted the request and is raising the AP. The SSID
    /// follows in a separate `0x25` ~2.5 s later.
    ///
    /// DEVICE-CONFIRMED that the ack payload is `01` and NOT the `0x30`/`0x31` mode byte we
    /// sent (`ac 55 00 03 39 01 3a`, `ac 55 00 03 67 01 68`) — so this is an
    /// acknowledgement, not an echo, and code that reads back its own mode from it is
    /// reading a constant.
    WifiOpening { serves: WifiService, raw: u8 },
    /// `0x45`.
    ActionSync(ActionSync),
    /// Something happened to the wake-word capture — `0x97` opened it, a `0x46` packet
    /// arrived, or it closed.
    ///
    /// The framing is [`crate::voice`]'s, not this module's: it is a state machine (a
    /// `0x46` with no preceding `0x97` recovers the lost start rather than dropping the
    /// utterance), and it is also where the Opus TOC lives. Emitted by [`Parser`], which
    /// owns the [`VoiceStream`]; [`parse`] is stateless and hands voice frames back as
    /// [`Self::VoiceFrame`] instead.
    Voice(VoiceEvent),
    /// A `0x46` / `0x97` / `0x99` frame reached [`parse`], which cannot decode one on its
    /// own. A routing instruction, not a decode: give it to a [`VoiceStream`], or use
    /// [`Parser`], which does exactly that and emits [`Self::Voice`].
    VoiceFrame { cmd: u8, data: Vec<u8> },
    /// `0x49` — the device abandoned the capture. CLIENT-ONLY, never observed. [`Parser`]
    /// drops the open capture on it, since the audio it was collecting is gone.
    VoiceAbandoned,
    /// `0x51`. CLIENT-ONLY, never observed.
    AiBroadcastCancelled,
    /// `0x52`. CLIENT-ONLY, never observed. Unrelated to the `52 58` file-frame magic.
    HdImageFailed,
    /// `0x96`. CLIENT-ONLY, never observed.
    IspUpgradeFinished { ok: bool },
    /// A frame whose opcode we know but that carries no state worth a type — the verbatim
    /// echo of a setter (`0x22`, `0x23`, `0x34`, `0x56`, `0x59`, `0x44`…) or a probe reply
    /// (`0x36` echoes the index it was given). The payload is kept because for `0x36` it is
    /// the entire result.
    Ack { command: AppCommand, data: Vec<u8> },
    /// The layout did not hold. Carries the raw frame so a shell can log exactly what
    /// arrived — the alternative, dropping it, is how a frozen battery readout produces no
    /// evidence at all.
    Malformed {
        cmd: u8,
        data: Vec<u8>,
        reason: MalformedReason,
    },
    /// A `cmd` byte in neither opcode table. A real answer, not a failure: the firmware
    /// under test drops opcodes it does not know, so probing an unknown one and reading its
    /// reply here is the documented characterisation method.
    Unrecognised { cmd: u8, data: Vec<u8> },
}

// ---------------------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------------------

/// Decode one inbound frame. Total, stateless, allocation-light.
///
/// Stateless means the three capture-machine opcodes come back as
/// [`DeviceEvent::VoiceFrame`] rather than decoded: the lost-start recovery needs memory,
/// and that memory belongs to [`crate::voice`], which [`Parser`] drives. Splitting it this
/// way keeps the field decode checkable against a capture line by line, which is how every
/// offset below was verified.
pub fn parse(frame: &Frame) -> DeviceEvent {
    let d = frame.data.as_slice();
    match frame.cmd {
        op::BATTERY_READ => battery_read(d),
        op::BATTERY_PUSH => battery_push(d),
        op::VERSIONS => versions(d),
        op::IDENTITY => identity(d),
        op::MEDIA_COUNT => media_count(d, CountSource::Push),
        op::FILE_COUNT_READ => media_count(d, CountSource::ReadReply),
        op::LED => led(d),
        op::RECORD_DURATION => record_duration(d),
        op::WEAR_DETECTION => switch_flag(d, SwitchReport::WearDetection, op::WEAR_DETECTION),
        op::VOICE_COMMAND => switch_flag(d, SwitchReport::VoiceCommand, op::VOICE_COMMAND),
        op::ORIENTATION => orientation(d),
        op::GESTURE_SWIPE_FORWARD
        | op::GESTURE_SWIPE_BACK
        | op::GESTURE_SINGLE_TAP
        | op::GESTURE_DOUBLE_TAP
        | op::GESTURE_TRIPLE_TAP => gesture(frame.cmd, d),
        op::CAPABILITIES => capabilities(d),
        op::VOICE_FEATURES => voice_features(d),
        op::VOLUMES => volumes(d),
        op::WIFI_NAME => wifi_credentials(d),
        op::OPEN_WIFI_FILES => wifi_opening(WifiService::Files, d, op::OPEN_WIFI_FILES),
        op::OPEN_WIFI_LIVE => wifi_opening(WifiService::Live, d, op::OPEN_WIFI_LIVE),
        op::ACTION_SYNC => action_sync(d),
        // The three stateful ones go back to the caller for [`VoiceStream`] to drive.
        op::VOICE_DATA | op::VOICE_START | op::VOICE_END => DeviceEvent::VoiceFrame {
            cmd: frame.cmd,
            data: frame.data.clone(),
        },
        op::ABANDON_VOICE => DeviceEvent::VoiceAbandoned,
        op::CANCEL_AI_BROADCAST => DeviceEvent::AiBroadcastCancelled,
        op::HD_IMAGE_FAILED => DeviceEvent::HdImageFailed,
        op::ISP_UPGRADE_DONE => DeviceEvent::IspUpgradeFinished {
            ok: d.first() == Some(&0x01),
        },
        other => match AppCommand::from_code(other) {
            Some(command) => DeviceEvent::Ack {
                command,
                data: frame.data.clone(),
            },
            None => DeviceEvent::Unrecognised {
                cmd: other,
                data: frame.data.clone(),
            },
        },
    }
}

fn too_short(cmd: u8, data: &[u8], need: usize) -> DeviceEvent {
    DeviceEvent::Malformed {
        cmd,
        data: data.to_vec(),
        reason: MalformedReason::TooShort { need },
    }
}

fn out_of_range(cmd: u8, data: &[u8]) -> DeviceEvent {
    DeviceEvent::Malformed {
        cmd,
        data: data.to_vec(),
        reason: MalformedReason::OutOfRange,
    }
}

fn not_text(cmd: u8, data: &[u8]) -> DeviceEvent {
    DeviceEvent::Malformed {
        cmd,
        data: data.to_vec(),
        reason: MalformedReason::NotText,
    }
}

/// `0x17` — `<tens><ones><charging>`, digits carried as `0x30 + n`.
///
/// The digits are NOT guaranteed to be `'0'`…`'9'`: at 100 % the firmware emits `3A 30`,
/// i.e. `0x30 + 10` for the tens place, which is a `'9' + 1` overflow it makes no attempt
/// to avoid. Decoding arithmetically rather than through a text parse is the whole fix —
/// DEVICE-CONFIRMED in `client_1`, `client_2` and `client_3`, where the unit sat at 100 % for
/// 44 consecutive replies.
fn battery_read(d: &[u8]) -> DeviceEvent {
    if d.len() < 3 {
        return too_short(op::BATTERY_READ, d, 3);
    }
    let (Some(tens), Some(ones)) = (d[0].checked_sub(b'0'), d[1].checked_sub(b'0')) else {
        return out_of_range(op::BATTERY_READ, d);
    };
    // `tens` reaches 10 only for 100 %; `ones` is a real digit. Both bounds are needed —
    // 10 tens with a non-zero ones place is not 10x%, it is a frame we do not understand.
    if tens > 10 || ones > 9 {
        return out_of_range(op::BATTERY_READ, d);
    }
    let percent = tens * 10 + ones;
    if percent > 100 {
        return out_of_range(op::BATTERY_READ, d);
    }
    DeviceEvent::Battery(BatteryReading {
        percent,
        charging: d[2] == 0x01,
        source: BatterySource::ReadReply,
    })
}

/// `0x53` — `<charging><percent>`, percent a RAW byte, fields in the OPPOSITE order to
/// `0x17`. Getting the order backwards produces a battery of 0 or 1 % that tracks the
/// charger, which is the shape of bug a smoke test misses.
fn battery_push(d: &[u8]) -> DeviceEvent {
    if d.len() < 2 {
        return too_short(op::BATTERY_PUSH, d, 2);
    }
    if d[1] > 100 {
        return out_of_range(op::BATTERY_PUSH, d);
    }
    DeviceEvent::Battery(BatteryReading {
        percent: d[1],
        charging: d[0] == 0x01,
        source: BatterySource::Push,
    })
}

fn versions(d: &[u8]) -> DeviceEvent {
    if d.len() < 7 {
        return too_short(op::VERSIONS, d, 7);
    }
    DeviceEvent::Versions(Versions {
        bt: [d[0], d[1], d[2]],
        isp: [d[3], d[4], d[5]],
        hardware: d[6],
    })
}

fn identity(d: &[u8]) -> DeviceEvent {
    if d.len() < 8 {
        return too_short(op::IDENTITY, d, 8);
    }
    let (Some(project), Some(customer)) = (ascii_field(&d[0..4]), ascii_field(&d[4..8])) else {
        return not_text(op::IDENTITY, d);
    };
    // Both blank means the payload told us nothing; one blank is a device whose SKU leaves
    // a field empty, which is not our business to reject.
    if project.is_empty() && customer.is_empty() {
        return not_text(op::IDENTITY, d);
    }
    DeviceEvent::Identity(Identity { project, customer })
}

fn media_count(d: &[u8], source: CountSource) -> DeviceEvent {
    let cmd = match source {
        CountSource::Push => op::MEDIA_COUNT,
        CountSource::ReadReply => op::FILE_COUNT_READ,
    };
    if d.len() < 2 {
        return too_short(cmd, d, 2);
    }
    DeviceEvent::MediaCount(MediaCount {
        count: u16::from_be_bytes([d[0], d[1]]),
        source,
    })
}

fn led(d: &[u8]) -> DeviceEvent {
    let Some(b) = d.first() else {
        return too_short(op::LED, d, 1);
    };
    let level = setting_digit(*b);
    if level > 2 {
        return out_of_range(op::LED, d);
    }
    DeviceEvent::Switch(SwitchReport::Led(level))
}

fn record_duration(d: &[u8]) -> DeviceEvent {
    if d.len() < 2 {
        return too_short(op::RECORD_DURATION, d, 2);
    }
    DeviceEvent::Switch(SwitchReport::RecordSeconds(u16::from_be_bytes([d[0], d[1]])))
}

fn switch_flag(d: &[u8], make: fn(bool) -> SwitchReport, cmd: u8) -> DeviceEvent {
    let Some(b) = d.first() else {
        return too_short(cmd, d, 1);
    };
    match setting_flag(*b) {
        Some(on) => DeviceEvent::Switch(make(on)),
        None => out_of_range(cmd, d),
    }
}

fn orientation(d: &[u8]) -> DeviceEvent {
    let Some(b) = d.first() else {
        return too_short(op::ORIENTATION, d, 1);
    };
    match setting_flag(*b) {
        Some(true) => DeviceEvent::Switch(SwitchReport::Orientation(Orientation::Landscape)),
        Some(false) => DeviceEvent::Switch(SwitchReport::Orientation(Orientation::Portrait)),
        None => out_of_range(op::ORIENTATION, d),
    }
}

fn gesture(cmd: u8, d: &[u8]) -> DeviceEvent {
    let Some(slot) = GestureSlot::from_code(cmd) else {
        // Unreachable while the dispatch and `GestureSlot::ALL` agree; reported rather
        // than panicked because a decode has no business aborting a shell's radio thread.
        return DeviceEvent::Unrecognised {
            cmd,
            data: d.to_vec(),
        };
    };
    let Some(raw) = d.first().copied() else {
        return too_short(cmd, d, 1);
    };
    DeviceEvent::GestureBinding {
        slot,
        action: GestureAction::from_value(raw),
        raw,
    }
}

fn capabilities(d: &[u8]) -> DeviceEvent {
    if d.len() < 4 {
        return too_short(op::CAPABILITIES, d, 4);
    }
    DeviceEvent::Capabilities(Capabilities {
        raw: u32::from_be_bytes([d[0], d[1], d[2], d[3]]),
    })
}

fn voice_features(d: &[u8]) -> DeviceEvent {
    match d.first() {
        Some(raw) => DeviceEvent::VoiceFeatures(VoiceFeatures { raw: *raw }),
        None => too_short(op::VOICE_FEATURES, d, 1),
    }
}

fn volumes(d: &[u8]) -> DeviceEvent {
    if d.len() < 3 {
        return too_short(op::VOLUMES, d, 3);
    }
    DeviceEvent::Volumes(Volumes {
        system: d[0],
        media: d[1],
        call: d[2],
    })
}

fn wifi_credentials(d: &[u8]) -> DeviceEvent {
    match ascii_field(d) {
        Some(ssid) if !ssid.is_empty() => DeviceEvent::WifiCredentials(WifiCredentials { ssid }),
        // An SSID that is empty once cleaned cannot be joined; surfacing it as text would
        // send the shell at a network that does not exist.
        _ => not_text(op::WIFI_NAME, d),
    }
}

fn wifi_opening(serves: WifiService, d: &[u8], cmd: u8) -> DeviceEvent {
    match d.first() {
        Some(raw) => DeviceEvent::WifiOpening { serves, raw: *raw },
        None => too_short(cmd, d, 1),
    }
}

fn action_sync(d: &[u8]) -> DeviceEvent {
    // Exactly ten, which is what all 85 captured frames carry. A nine-byte frame — the
    // layout the PDF describes — is reported rather than partially decoded: nobody has
    // seen one, and guessing which of the two layouts a short frame is written in is the
    // failure this decode exists to stop.
    if d.len() < 10 {
        return too_short(op::ACTION_SYNC, d, 10);
    }
    let mut flags = [0u8; 10];
    flags.copy_from_slice(&d[..10]);
    DeviceEvent::ActionSync(ActionSync::from_flags(&flags))
}

/// A settings value byte as a number.
///
/// Every captured single-byte setting is an ASCII digit; a raw `0`/`1`/`2` is accepted too
/// so a firmware variant that omits the offset still decodes. The two cannot collide in
/// practice — no setting takes a value near 48.
fn setting_digit(byte: u8) -> u8 {
    if byte.is_ascii_digit() {
        byte - b'0'
    } else {
        byte
    }
}

/// A two-valued setting.
///
/// **Deliberate divergence from both reference implementations**, which read any non-zero digit as "on". A
/// binary setting reporting `'2'` is not on, it is a frame we do not understand, and
/// reporting it as on is the same class of mistake as clamping a battery percentage. Every
/// captured value is `'0'` or `'1'`, so the divergence changes nothing on evidence we hold.
fn setting_flag(byte: u8) -> Option<bool> {
    match setting_digit(byte) {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// A NUL-padded ASCII field: cut at the first NUL, drop surrounding whitespace and control
/// bytes, reject anything that is not ASCII.
///
/// The cleaning is DEFENSIVE, not observed: the captured `0x25` SSID is 13 bare ASCII bytes
/// with no terminator and no padding, and the captured `0x64` identity is the only field
/// that genuinely uses NUL padding (`54 31 00 00`). It stays because iOS's
/// `NEHotspotConfiguration` silently rejects an SSID containing any control character, so a
/// padded variant would fail the join with no diagnosis.
fn ascii_field(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let head = &bytes[..end];
    if !head.is_ascii() {
        return None;
    }
    let text = String::from_utf8(head.to_vec()).ok()?;
    Some(
        text.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control())
            .to_string(),
    )
}

// ---------------------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------------------

/// The inbound stream, from notification bytes to typed events.
///
/// Three pieces, none of them reimplemented here: a [`Deframer`] for the envelope,
/// [`parse`] for the fields, and a [`VoiceStream`] for the one decode that needs memory.
/// The voice machine is [`crate::voice`]'s on purpose — the lost-`0x97` recovery rule
/// existed once in each reference implementation and must not now exist twice here.
///
/// Sans-IO: no timer, no clock, no queue. The reference iOS implementation's 1.2 s idle flush and 30 s
/// open-mic cap are client policy and are absent here — this type can tell a
/// shell that a capture is open ([`Self::is_capturing`]) and be told when the shell ended
/// it ([`Self::close_voice`]), which is the whole of what this crate can honestly know
/// without measuring time.
#[derive(Debug, Clone)]
pub struct Parser {
    deframer: Deframer,
    voice: VoiceStream,
}

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            deframer: Deframer::device(),
            voice: VoiceStream::new(),
        }
    }

    /// True while a voice capture is open.
    pub fn is_capturing(&self) -> bool {
        self.voice.is_capturing()
    }

    /// Bytes held that have not yet completed a frame.
    pub fn buffered(&self) -> &[u8] {
        self.deframer.buffered()
    }

    /// End the capture for a reason only the shell knows.
    ///
    /// It has to be told to us, and this is the method that matters most on this firmware:
    /// `0x56` is an app→device WRITE that never appears on the stream we parse, and it is
    /// the only thing that closes the mic — four captures totalling 14,636 voice frames
    /// contain no `0x99` and no gap. A parser that waits to observe the close waits
    /// forever. [`EndCause::IdleTimeout`] and [`EndCause::Disconnected`] are the shell's
    /// other two reasons; the DURATION behind the first is the shell's alone.
    pub fn close_voice(&mut self, cause: EndCause) -> Vec<DeviceEvent> {
        self.voice.close(cause).into_iter().map(DeviceEvent::Voice).collect()
    }

    /// Forget the frame buffer and the capture state. The shell calls this on disconnect,
    /// where a half-frame from the previous link would otherwise prefix the next one and a
    /// stale "capturing" would swallow the next session's recovered start.
    ///
    /// Prefer [`Self::close_voice`] with [`EndCause::Disconnected`] when a capture was
    /// open — it says what happened instead of discarding it silently.
    pub fn reset(&mut self) {
        self.deframer.reset();
        self.voice.reset();
    }

    /// Feed one notification's bytes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<DeviceEvent> {
        self.push_detailed(chunk).0
    }

    /// [`Self::push`], plus what happened to the bytes that did not become frames.
    ///
    /// The residual matters on `AA15`, where `52 58` file bytes share the characteristic:
    /// [`Residual::Foreign`] is the file stream and must be routed on, not dropped.
    pub fn push_detailed(&mut self, chunk: &[u8]) -> (Vec<DeviceEvent>, Residual) {
        let (frames, residual) = self.deframer.push_detailed(chunk);
        let mut out = Vec::with_capacity(frames.len());
        for frame in &frames {
            self.feed_into(frame, &mut out);
        }
        (out, residual)
    }

    /// Decode one already-deframed frame, applying the voice-capture state machine.
    ///
    /// Returns 0, 1 or 2 events: two when a lost `0x97` is recovered, zero when a `0x99`
    /// arrives with no capture open.
    pub fn feed(&mut self, frame: &Frame) -> Vec<DeviceEvent> {
        let mut out = Vec::new();
        self.feed_into(frame, &mut out);
        out
    }

    fn feed_into(&mut self, frame: &Frame, out: &mut Vec<DeviceEvent>) {
        match parse(frame) {
            // The voice machine is the one decode with memory, and it is not ours — see
            // [`crate::voice`]. Handing the whole frame over keeps the lost-start recovery
            // and the packet indices in exactly one place.
            DeviceEvent::VoiceFrame { .. } => {
                out.extend(self.voice.handle(frame).into_iter().map(DeviceEvent::Voice));
            }
            DeviceEvent::VoiceAbandoned => {
                // The audio the capture was collecting is gone, so the machine is reset
                // rather than closed: the end has already been reported, as this event.
                self.voice.reset();
                out.push(DeviceEvent::VoiceAbandoned);
            }
            other => out.push(other),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Switch-state accumulation
// ---------------------------------------------------------------------------------------

/// Record durations a device may plausibly report, in seconds.
///
/// A value outside this is not adopted. The reason is specific: on iOS the reported
/// duration lands in `@AppStorage` and is then pushed BACK to the device by the settings
/// slider, so one corrupt frame writes a corrupt setting to the hardware permanently.
/// Device-reported values so far are 60, 90 and 180.
pub const PLAUSIBLE_RECORD_SECONDS: std::ops::RangeInclusive<u16> = 1..=3600;

/// Accumulates the `0x48` burst — and the unsolicited echo of any setter write — into one
/// settings snapshot.
///
/// The device emits ten frames in ~5 ms (see
/// [`SWITCH_STATE_BURST_ORDER`](crate::SWITCH_STATE_BURST_ORDER)), so this fills in one go;
/// it accepts them in any order anyway, because a later single-setting echo arrives alone.
///
/// **[`Self::is_complete`] counts the five gestures.** The iOS accumulator does not — it
/// predates the gesture opcodes being identified — so its `deviceSettings` reports complete
/// while the entire touch-input configuration is still unknown. Android does not model the
/// burst at all. Requiring all ten is what makes "settings synced" mean it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwitchStates {
    led: Option<u8>,
    record_seconds: Option<u16>,
    wear_detection: Option<bool>,
    voice_command: Option<bool>,
    orientation: Option<Orientation>,
    /// Indexed by position in [`GestureSlot::ALL`].
    gestures: [Option<u8>; 5],
}

impl SwitchStates {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event in. Returns true when the event was a settings frame, so a caller
    /// routing a stream knows whether it still has to handle it.
    pub fn ingest(&mut self, event: &DeviceEvent) -> bool {
        match event {
            DeviceEvent::Switch(SwitchReport::Led(level)) => self.led = Some(*level),
            DeviceEvent::Switch(SwitchReport::RecordSeconds(secs)) => {
                if PLAUSIBLE_RECORD_SECONDS.contains(secs) {
                    self.record_seconds = Some(*secs);
                } else {
                    // Left unset on purpose: an implausible duration must not reach the
                    // shell's storage, and "not reported" is the honest state for a value
                    // we refuse to believe.
                    return true;
                }
            }
            DeviceEvent::Switch(SwitchReport::WearDetection(on)) => self.wear_detection = Some(*on),
            DeviceEvent::Switch(SwitchReport::VoiceCommand(on)) => self.voice_command = Some(*on),
            DeviceEvent::Switch(SwitchReport::Orientation(o)) => self.orientation = Some(*o),
            DeviceEvent::GestureBinding { slot, raw, .. } => {
                if let Some(i) = GestureSlot::ALL.iter().position(|s| s == slot) {
                    self.gestures[i] = Some(*raw);
                }
            }
            _ => return false,
        }
        true
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn led(&self) -> Option<u8> {
        self.led
    }

    pub fn record_seconds(&self) -> Option<u16> {
        self.record_seconds
    }

    pub fn wear_detection(&self) -> Option<bool> {
        self.wear_detection
    }

    pub fn voice_command(&self) -> Option<bool> {
        self.voice_command
    }

    pub fn orientation(&self) -> Option<Orientation> {
        self.orientation
    }

    /// The action a slot is bound to, or `None` when the slot has not been reported or its
    /// value is outside the known set.
    pub fn gesture(&self, slot: GestureSlot) -> Option<GestureAction> {
        self.gesture_raw(slot).and_then(GestureAction::from_value)
    }

    /// The slot's value byte exactly as reported, even when it maps to no known action.
    pub fn gesture_raw(&self, slot: GestureSlot) -> Option<u8> {
        GestureSlot::ALL
            .iter()
            .position(|s| *s == slot)
            .and_then(|i| self.gestures[i])
    }

    /// True once every field the burst reports has arrived — including all five gestures.
    pub fn is_complete(&self) -> bool {
        self.led.is_some()
            && self.record_seconds.is_some()
            && self.wear_detection.is_some()
            && self.voice_command.is_some()
            && self.orientation.is_some()
            && self.gestures.iter().all(Option::is_some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode_device, encode_device};

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(s.len().is_multiple_of(2), "hex literal must be byte-aligned");
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Decode a frame lifted verbatim from a capture, and assert on the way through that it
    /// re-encodes byte-for-byte — so a typo in a golden vector fails here rather than
    /// producing a confidently wrong event.
    fn event(frame_hex: &str) -> DeviceEvent {
        let bytes = hex(frame_hex);
        let frame = decode_device(&bytes).unwrap_or_else(|e| panic!("{frame_hex}: {e:?}"));
        assert_eq!(
            encode_device(frame.cmd, &frame.data),
            bytes,
            "{frame_hex} does not round-trip"
        );
        parse(&frame)
    }

    fn battery(e: DeviceEvent) -> BatteryReading {
        match e {
            DeviceEvent::Battery(b) => b,
            other => panic!("expected a battery reading, got {other:?}"),
        }
    }

    // -- battery ------------------------------------------------------------------------

    /// The strongest evidence in this file: two different encodings of the SAME quantity,
    /// captured from the same units minutes apart, agreeing.
    ///
    /// `eyevue_1` charging in the high forties, `eyevue_2` charging in the mid sixties,
    /// `glassx_1` discharging through the low twenties — in each capture the ASCII `0x17`
    /// reply and the raw `0x53` push track one another. A field-order or encoding mistake
    /// in either decoder breaks the agreement, which is why this is one test and not two.
    #[test]
    fn the_two_battery_encodings_agree_across_three_captures() {
        // (0x17 read-reply frame, 0x53 push frame, percent, charging, capture) — every
        // string below is verbatim from a capture, both columns.
        let pairs: &[(&str, &str, u8, bool, &str)] = &[
            ("ac5500051734390185", "ac55000453013185", 49, true, "eyevue_1"),
            ("ac550005173530017d", "ac55000453013286", 50, true, "eyevue_1"),
            ("ac5500051736340182", "ac55000453014094", 64, true, "eyevue_2"),
            ("ac550005173232007b", "ac55000453001669", 22, false, "glassx_1"),
        ];
        for (read, push, percent, charging, capture) in pairs {
            let r = battery(event(read));
            assert_eq!(r.percent, *percent, "{capture} read reply");
            assert_eq!(r.charging, *charging, "{capture} read reply");
            assert_eq!(r.source, BatterySource::ReadReply);

            let p = battery(event(push));
            assert_eq!(p.percent, *percent, "{capture} push");
            assert_eq!(p.charging, *charging, "{capture} push");
            assert_eq!(p.source, BatterySource::Push);

            // The encodings differ in every respect except the answer. The read carries
            // the percentage as two `0x30 + n` digits with charging LAST; the push carries
            // it as one raw byte with charging FIRST. Reading either layout with the
            // other's rules yields a number that still looks like a battery.
            let (read, push) = (hex(read), hex(push));
            assert_eq!((read[5] - b'0') * 10 + (read[6] - b'0'), *percent);
            assert_eq!(read[7], u8::from(*charging));
            assert_eq!(push[6], *percent);
            assert_eq!(push[5], u8::from(*charging));
        }
    }

    /// Every distinct `0x53` push in the captures, decoded. Three units, two charge
    /// directions, eleven distinct payloads — and the percent is a RAW byte in all of them,
    /// which is the field `0x17` encodes as ASCII.
    #[test]
    fn every_captured_battery_push_decodes_with_the_fields_in_push_order() {
        let cases: &[(&str, u8, bool)] = &[
            ("ac55000453013185", 49, true),  // eyevue_1
            ("ac55000453013286", 50, true),  // eyevue_1
            ("ac55000453013387", 51, true),  // eyevue_1
            ("ac55000453014094", 64, true),  // eyevue_2
            ("ac55000453014195", 65, true),  // eyevue_2
            ("ac55000453014296", 66, true),  // eyevue_2
            ("ac55000453001467", 20, false), // glassx_1
            ("ac55000453001568", 21, false), // glassx_1
            ("ac55000453001669", 22, false), // glassx_1
            ("ac5500045300176a", 23, false), // glassx_1
            ("ac5500045300186b", 24, false), // glassx_1
        ];
        for (h, percent, charging) in cases {
            let b = battery(event(h));
            assert_eq!((b.percent, b.charging), (*percent, *charging), "{h}");
        }
    }

    /// The bug that froze the readout. At 100 % the tens digit overflows `'9'` to `0x3A`;
    /// a text parse returns nothing for `':'` and drops the frame, so the battery sticks at
    /// whatever it last read. 44 consecutive replies in `client_1`/`_2`/`_3` are this frame.
    #[test]
    fn one_hundred_percent_arrives_as_a_digit_overflow_and_still_decodes() {
        let b = battery(event("ac550005173a300182"));
        assert_eq!(b.percent, 100);
        assert!(b.charging);
        // The tens byte is not an ASCII digit at all — that is the whole point.
        assert!(!hex("ac550005173a300182")[5].is_ascii_digit());
    }

    /// Out of range is [`DeviceEvent::Malformed`], never a clamp. A clamp shows the user a
    /// plausible wrong number; a Malformed shows the shell the bytes.
    #[test]
    fn an_impossible_battery_percentage_is_malformed_rather_than_clamped() {
        // 0x17: tens = 10, ones = 5 → 105 %.
        let bytes = encode_device(op::BATTERY_READ, &[0x3A, 0x35, 0x01]);
        assert!(matches!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                reason: MalformedReason::OutOfRange,
                ..
            }
        ));
        // 0x17: a byte below '0' would underflow a naive subtraction.
        let bytes = encode_device(op::BATTERY_READ, &[0x20, 0x30, 0x00]);
        assert!(matches!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed { .. }
        ));
        // 0x53: raw 200 %.
        let bytes = encode_device(op::BATTERY_PUSH, &[0x01, 200]);
        assert!(matches!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                reason: MalformedReason::OutOfRange,
                ..
            }
        ));
        // Short payloads say so, and say how short.
        let bytes = encode_device(op::BATTERY_READ, &[0x34, 0x39]);
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                cmd: 0x17,
                data: vec![0x34, 0x39],
                reason: MalformedReason::TooShort { need: 3 },
            }
        );
    }

    // -- identity, versions, capabilities -----------------------------------------------

    #[test]
    fn the_captured_version_and_identity_frames_decode_to_the_unit_under_test() {
        assert_eq!(
            event("ac550009550104080103010269"),
            DeviceEvent::Versions(Versions {
                bt: [1, 4, 8],
                isp: [1, 3, 1],
                hardware: 2,
            })
        );
        // 8 bytes, 4 + 4, NUL-padded: "T1\0\0" then "0303".
        assert_eq!(
            event("ac55000a645431000030333033af"),
            DeviceEvent::Identity(Identity {
                project: "T1".into(),
                customer: "0303".into(),
            })
        );
    }

    /// The capability word is carried, not interpreted. Seventeen frames across six
    /// captures, one distinct value — which pins the hardware, not the bit meanings.
    #[test]
    fn the_capability_word_is_kept_opaque() {
        let e = event("ac550006950000040ba4");
        let DeviceEvent::Capabilities(c) = e else {
            panic!("expected capabilities, got {e:?}");
        };
        assert_eq!(c.raw, 0x0000_040B);
        assert_eq!(c.raw, Capabilities::KNOWN_E09);
        assert!(c.is_known_configuration());
        // Big-endian, and it matters: read the other way this word is 0x0B040000.
        assert_ne!(c.raw, u32::from_le_bytes([0x00, 0x00, 0x04, 0x0B]));
        // Four bits are set and the vendor shows four true flags — recorded as an
        // observation, deliberately not turned into a mapping.
        assert_eq!(c.raw.count_ones(), 4);
        // A different unit is not an error, just an unknown configuration.
        assert!(!Capabilities { raw: 0 }.is_known_configuration());
    }

    #[test]
    fn the_voice_disable_bitmap_is_reported_as_all_enabled_only_at_zero() {
        assert_eq!(
            event("ac550003710071"),
            DeviceEvent::VoiceFeatures(VoiceFeatures { raw: 0 })
        );
        assert!(VoiceFeatures { raw: 0 }.all_enabled());
        assert!(!VoiceFeatures { raw: 0x01 }.all_enabled());
    }

    // -- counts and volumes -------------------------------------------------------------

    /// Two-byte BIG-endian, taken from seven captures. `0x0012` is 18, not 4,608.
    #[test]
    fn captured_media_counts_are_big_endian() {
        for (h, n) in [
            ("ac55000442000648", 6u16),
            ("ac55000442000749", 7),
            ("ac5500044200084a", 8),
            ("ac55000442000f51", 15),
            ("ac55000442001052", 16),
            ("ac55000442001254", 18),
            ("ac55000442001355", 19),
        ] {
            assert_eq!(
                event(h),
                DeviceEvent::MediaCount(MediaCount {
                    count: n,
                    source: CountSource::Push,
                }),
                "{h}"
            );
        }
    }

    /// `0x40` is written eight times across the captures and never comes back. The branch
    /// stays, labelled — a shell can tell an inferred reply from an observed push.
    #[test]
    fn the_file_count_read_reply_is_labelled_inferred_because_no_capture_holds_one() {
        let bytes = encode_device(op::FILE_COUNT_READ, &[0x00, 0x0F]);
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::MediaCount(MediaCount {
                count: 15,
                source: CountSource::ReadReply,
            })
        );
    }

    /// The reply ORDER is what this pins. `eyevue_1` wrote `70 01 07`, `70 00 06`,
    /// `70 02 06` — media, system, call — and read back `06 07 06`, which is
    /// `[system, media, call]` and not the order the writes went out in.
    #[test]
    fn the_captured_volume_reply_is_system_media_call() {
        let e = event("ac550005690607067c");
        let DeviceEvent::Volumes(v) = e else {
            panic!("expected volumes, got {e:?}");
        };
        assert_eq!((v.system, v.media, v.call), (6, 7, 6));
        assert_eq!(v.level(VolumeChannel::System), 6);
        assert_eq!(v.level(VolumeChannel::Media), 7);
        assert_eq!(v.level(VolumeChannel::Call), 6);
        // The scale is not normalised. Captured levels run to 0x10 with no maximum ever
        // observed, so dividing by a guessed maximum here would be a slider that lies.
        let e = event("ac550005690c100f94");
        assert_eq!(
            e,
            DeviceEvent::Volumes(Volumes {
                system: 0x0C,
                media: 0x10,
                call: 0x0F,
            })
        );
    }

    // -- Wi-Fi ---------------------------------------------------------------------------

    /// The SSID payload as it really arrives: 13 bare ASCII bytes, `len = 0x0F`, no NUL
    /// terminator and no padding — in all four captures that contain one. The vendor PDF and
    /// both reference implementations say NUL-terminated; that is defensive, and this test says
    /// which is which.
    #[test]
    fn the_captured_ssid_has_no_nul_terminator_despite_the_spec() {
        let bytes = hex("ac55000f2544482d5477492d34363431453467");
        let frame = decode_device(&bytes).expect("ssid frame decodes");
        assert_eq!(frame.data.len(), 13);
        assert!(!frame.data.contains(&0), "no terminator on this firmware");

        let e = parse(&frame);
        let DeviceEvent::WifiCredentials(w) = e else {
            panic!("expected credentials, got {e:?}");
        };
        assert_eq!(w.ssid, "DH-TwI-4641E4");
        // The passphrase is not on the wire; it is fixed on this firmware and lives with
        // the SSID so the joining shell carries no magic string of its own.
        assert_eq!(w.passphrase(), "12345678");

        // A NUL-padded variant — which no capture holds — decodes to the same SSID, which
        // is the entire reason the defensive cleaning is kept.
        let padded = encode_device(op::WIFI_NAME, b"DH-TwI-4641E4\0\0\0");
        assert_eq!(parse(&decode_device(&padded).unwrap()), DeviceEvent::WifiCredentials(w));
    }

    #[test]
    fn an_ssid_that_cleans_to_nothing_is_malformed_rather_than_joinable() {
        let bytes = encode_device(op::WIFI_NAME, &[0x00, 0x00, 0x00]);
        assert!(matches!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                reason: MalformedReason::NotText,
                ..
            }
        ));
    }

    /// Both Wi-Fi opens ack with `01`, NOT the `0x30`/`0x31` mode byte the app sent. Code
    /// that reads its own mode back out of the ack is reading a constant, and the two
    /// captures below are what says so.
    #[test]
    fn the_wifi_open_ack_is_zero_one_not_the_mode_byte_we_sent() {
        assert_eq!(
            event("ac55000339013a"),
            DeviceEvent::WifiOpening {
                serves: WifiService::Files,
                raw: 0x01,
            }
        );
        assert_eq!(
            event("ac550003670168"),
            DeviceEvent::WifiOpening {
                serves: WifiService::Live,
                raw: 0x01,
            }
        );
        assert_eq!(WifiService::Files.command(), AppCommand::OpenWifiFiles);
        assert_eq!(WifiService::Live.command(), AppCommand::OpenWifiLive);
    }

    // -- action sync ---------------------------------------------------------------------

    /// Real action-sync frames, one per pattern the captures contain. Ten bytes every time
    /// — the PDF's nine would put `importingMode` off the end of the frame, which is
    /// exactly what the reference Android implementation's nine-flag reader does. Index 0 is here too: Android
    /// names `takePhoto` in a comment and never assigns it.
    #[test]
    fn every_captured_action_sync_pattern_decodes_to_the_flags_that_fired() {
        let cases: &[(&str, &[DeviceAction], &str)] = &[
            ("ac55000c450000000000000000000045", &[], "idle, 35 frames"),
            (
                "ac55000c450100000000000000000046",
                &[DeviceAction::TakePhoto],
                "photo — eyevue_1, eyevue_2, glassx_1",
            ),
            (
                "ac55000c450001000000000000000046",
                &[DeviceAction::AudioRecord],
                "voice recording — eyevue_1, eyevue_2, glassx_1",
            ),
            (
                "ac55000c450000010000000000000046",
                &[DeviceAction::VideoRecord],
                "video — eyevue_1",
            ),
            (
                "ac55000c450000000000000001000046",
                &[DeviceAction::PlayPause],
                "music playing — 17 frames, 5 captures",
            ),
            (
                "ac55000c450000000000000000000146",
                &[DeviceAction::ImportingMode],
                "wi-fi session open — eyevue_1, eyevue_4",
            ),
            (
                "ac55000c450000000100000001000047",
                &[DeviceAction::VolumeUp, DeviceAction::PlayPause],
                "touchpad volume up while playing — eyevue_3, client_1",
            ),
            (
                "ac55000c450000000001000001000047",
                &[DeviceAction::VolumeDown, DeviceAction::PlayPause],
                "touchpad volume down while playing — eyevue_3, client_1",
            ),
        ];
        for (h, expect, why) in cases {
            let e = event(h);
            let DeviceEvent::ActionSync(a) = e else {
                panic!("{why}: expected action sync, got {e:?}");
            };
            assert_eq!(a.active(), *expect, "{why}");
        }
    }

    /// Three of the ten flags have never been set on any device we hold, across all 85
    /// captured frames. They stay in the enum — the byte position is real — but they are
    /// labelled, because a UI bound to one renders a state the hardware has never reported.
    #[test]
    fn the_three_action_flags_that_have_never_fired_are_labelled_client_only() {
        let unproven: Vec<DeviceAction> = DeviceAction::ALL
            .into_iter()
            .filter(|a| a.evidence() != Evidence::Capture)
            .collect();
        assert_eq!(
            unproven,
            vec![
                DeviceAction::NodHead,
                DeviceAction::ShakeHead,
                DeviceAction::WearingDetection,
            ]
        );
        // Index 9 is file-import mode, not "Wi-Fi active" — the last byte of the frame,
        // and the one a nine-byte reading of the PDF loses entirely.
        assert_eq!(DeviceAction::ImportingMode.index(), 9);
        assert_eq!(DeviceAction::from_index(9), Some(DeviceAction::ImportingMode));
        assert_eq!(DeviceAction::from_index(10), None);
    }

    /// A short action-sync frame is reported, not partially decoded. The PDF describes a
    /// nine-field layout; no capture holds one, and guessing which layout a short frame
    /// follows is the exact failure a total decode exists to stop.
    #[test]
    fn a_nine_byte_action_sync_is_malformed_rather_than_guessed_at() {
        let bytes = encode_device(op::ACTION_SYNC, &[0u8; 9]);
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                cmd: 0x45,
                data: vec![0; 9],
                reason: MalformedReason::TooShort { need: 10 },
            }
        );
    }

    // -- switch states and gestures ------------------------------------------------------

    /// The whole `0x48` burst as six devices send it, folded into one snapshot — including
    /// the five gesture slots the reference Android implementation has no opcodes for.
    ///
    /// This is the concrete payoff of the port: ten frames in, one complete settings state
    /// out, on both platforms, from one decode.
    #[test]
    fn the_captured_switch_burst_fills_every_field_including_all_five_gestures() {
        const BURST: &[&str] = &[
            "ac550003013132",   // LED '1'
            "ac55000402003c3e", // record 60 s
            "ac550003043135",   // wear detect '1'
            "ac550003063137",   // voice command '1'
            "ac550003073037",   // swipe forward '0'
            "ac550003083139",   // swipe back '1'
            "ac55000309323b",   // single tap '2'
            "ac550003103444",   // double tap '4'
            "ac550003113344",   // triple tap '3'
            "ac550003613091",   // orientation '0'
        ];
        let mut states = SwitchStates::new();
        let mut parser = Parser::new();
        let blob: Vec<u8> = BURST.iter().flat_map(|h| hex(h)).collect();
        let events = parser.push(&blob);
        assert_eq!(events.len(), 10, "the burst is ten frames");
        for e in &events {
            assert!(states.ingest(e), "every burst frame is a settings frame: {e:?}");
        }

        assert_eq!(states.led(), Some(1));
        assert_eq!(states.record_seconds(), Some(60));
        assert_eq!(states.wear_detection(), Some(true));
        assert_eq!(states.voice_command(), Some(true));
        assert_eq!(states.orientation(), Some(Orientation::Portrait));
        assert_eq!(states.gesture(GestureSlot::SwipeForward), Some(GestureAction::VolumeDown));
        assert_eq!(states.gesture(GestureSlot::SwipeBack), Some(GestureAction::VolumeUp));
        assert_eq!(states.gesture(GestureSlot::SingleTap), Some(GestureAction::PlayPause));
        assert_eq!(states.gesture(GestureSlot::DoubleTap), Some(GestureAction::PreviousTrack));
        assert_eq!(states.gesture(GestureSlot::TripleTap), Some(GestureAction::NextTrack));
        assert!(states.is_complete());
        // No frame in the burst is keyed 0x48 — the request opcode never comes back.
        assert!(!blob.windows(5).any(|w| w[0] == 0xAC && w[1] == 0x55 && w[4] == 0x48));
    }

    /// Completeness counts the gestures, which is where this diverges from the iOS
    /// accumulator: it reports "settings synced" with the whole touch-input configuration
    /// still unknown, because it predates the gesture opcodes being identified.
    #[test]
    fn a_burst_missing_its_gesture_frames_is_not_complete_here() {
        let mut states = SwitchStates::new();
        for h in [
            "ac550003013132",
            "ac55000402003c3e",
            "ac550003043135",
            "ac550003063137",
            "ac550003613091",
        ] {
            states.ingest(&event(h));
        }
        // Everything the iOS accumulator models is present…
        assert_eq!(states.led(), Some(1));
        assert_eq!(states.orientation(), Some(Orientation::Portrait));
        // …and we still do not know what a double tap does.
        assert!(!states.is_complete());
        assert_eq!(states.gesture(GestureSlot::DoubleTap), None);
    }

    /// The five gesture frames, decoded from the bytes six devices sent, with the slot ids
    /// asserted against the ones Android does not declare. These frames arrive on the
    /// Android wire today and are dropped.
    #[test]
    fn the_five_gesture_frames_decode_and_are_exactly_the_android_gap() {
        let cases: [(&str, GestureSlot, GestureAction); 5] = [
            ("ac550003073037", GestureSlot::SwipeForward, GestureAction::VolumeDown),
            ("ac550003083139", GestureSlot::SwipeBack, GestureAction::VolumeUp),
            ("ac55000309323b", GestureSlot::SingleTap, GestureAction::PlayPause),
            ("ac550003103444", GestureSlot::DoubleTap, GestureAction::PreviousTrack),
            ("ac550003113344", GestureSlot::TripleTap, GestureAction::NextTrack),
        ];
        for (h, slot, action) in cases {
            assert_eq!(
                event(h),
                DeviceEvent::GestureBinding {
                    slot,
                    action: Some(action),
                    raw: action.ascii(),
                },
                "{h}"
            );
        }
        // The captured defaults, in burst order, are the '0 1 2 4 3' every unit reports.
        let raws: Vec<u8> = cases.iter().map(|(_, _, a)| a.ascii()).collect();
        assert_eq!(raws, GestureSlot::CAPTURED_DEFAULTS);
    }

    /// A binding outside the known set keeps its byte and reports no action. Guessing here
    /// would tell the user their double tap skips a track when nobody knows what it does.
    #[test]
    fn an_unmapped_gesture_value_keeps_its_byte_instead_of_guessing() {
        let bytes = encode_device(op::GESTURE_DOUBLE_TAP, b"9");
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::GestureBinding {
                slot: GestureSlot::DoubleTap,
                action: None,
                raw: b'9',
            }
        );
        let mut states = SwitchStates::new();
        states.ingest(&parse(&decode_device(&bytes).unwrap()));
        assert_eq!(states.gesture(GestureSlot::DoubleTap), None);
        assert_eq!(states.gesture_raw(GestureSlot::DoubleTap), Some(b'9'));
    }

    /// All three LED levels are on the wire — `'0'`, `'1'` and `'2'` — which is what turns
    /// the PDF's "low/mid/high" into a complete observed range rather than a partial one.
    /// The feature is dead (no client drives it) but the frame is real and in every burst.
    #[test]
    fn all_three_captured_led_levels_decode_and_a_fourth_is_out_of_range() {
        assert_eq!(event("ac550003013031"), DeviceEvent::Switch(SwitchReport::Led(0)));
        assert_eq!(event("ac550003013132"), DeviceEvent::Switch(SwitchReport::Led(1)));
        assert_eq!(event("ac550003013233"), DeviceEvent::Switch(SwitchReport::Led(2)));
        let bytes = encode_device(op::LED, b"3");
        assert!(matches!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Malformed {
                reason: MalformedReason::OutOfRange,
                ..
            }
        ));
    }

    /// `0x02` is the one burst value that is a 2-byte integer rather than an ASCII digit.
    /// All three captured durations, plus the guard that stops a corrupt one being adopted
    /// and then written back to the hardware by the settings slider.
    #[test]
    fn record_duration_is_big_endian_seconds_and_implausible_values_are_not_adopted() {
        for (h, secs) in [
            ("ac55000402003c3e", 60u16),
            ("ac55000402005a5c", 90),
            ("ac5500040200b4b6", 180),
        ] {
            assert_eq!(
                event(h),
                DeviceEvent::Switch(SwitchReport::RecordSeconds(secs)),
                "{h}"
            );
        }
        let mut states = SwitchStates::new();
        // The decode is honest about what arrived…
        let bytes = encode_device(op::RECORD_DURATION, &[0xFF, 0xFF]);
        let e = parse(&decode_device(&bytes).unwrap());
        assert_eq!(e, DeviceEvent::Switch(SwitchReport::RecordSeconds(65535)));
        // …and the accumulator refuses to believe it, so it never reaches shell storage.
        assert!(states.ingest(&e));
        assert_eq!(states.record_seconds(), None);
        assert!(states.ingest(&event("ac55000402003c3e")));
        assert_eq!(states.record_seconds(), Some(60));
    }

    /// Two-valued settings take 0 or 1 and nothing else. Both reference implementations read any non-zero as
    /// "on"; a `'2'` in a binary field is a frame we do not understand, and reporting it as
    /// on is the same mistake as clamping a battery percentage.
    #[test]
    fn binary_settings_reject_a_value_that_is_neither_zero_nor_one() {
        assert_eq!(
            event("ac550003043135"),
            DeviceEvent::Switch(SwitchReport::WearDetection(true))
        );
        assert_eq!(
            event("ac550003063137"),
            DeviceEvent::Switch(SwitchReport::VoiceCommand(true))
        );
        assert_eq!(
            event("ac550003613091"),
            DeviceEvent::Switch(SwitchReport::Orientation(Orientation::Portrait))
        );
        // Landscape has never been captured — the encoding is the same field, inverted.
        let bytes = encode_device(op::ORIENTATION, b"1");
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Switch(SwitchReport::Orientation(Orientation::Landscape))
        );
        for cmd in [op::WEAR_DETECTION, op::VOICE_COMMAND, op::ORIENTATION] {
            let bytes = encode_device(cmd, b"2");
            assert!(
                matches!(
                    parse(&decode_device(&bytes).unwrap()),
                    DeviceEvent::Malformed {
                        reason: MalformedReason::OutOfRange,
                        ..
                    }
                ),
                "{cmd:#04x} accepted a third value"
            );
        }
        // A raw 0/1 decodes too, for a firmware variant that omits the ASCII offset.
        let bytes = encode_device(op::WEAR_DETECTION, &[0x00]);
        assert_eq!(
            parse(&decode_device(&bytes).unwrap()),
            DeviceEvent::Switch(SwitchReport::WearDetection(false))
        );
    }

    // -- voice ---------------------------------------------------------------------------

    /// A real voice frame from `client_1`, routed to the capture machine rather than decoded
    /// here — 40 payload bytes, carried verbatim, TOC parsed by [`crate::voice`]. Byte 0 of
    /// all 14,636 captured payloads is `0x4B` or `0x48`, an Opus TOC selecting SILK
    /// wideband 20 ms, which is what makes the spec's "格式为opus" an observation rather
    /// than a claim.
    #[test]
    fn a_captured_voice_frame_is_carried_verbatim_and_opens_a_capture() {
        const VOICE: &str = "ac55002a464b410516aed2dea71b251b88ee731dbd78b1d8b1c98f\
                             d34a354f35796e124b53043ab2000000000077";
        let mut parser = Parser::new();
        assert!(!parser.is_capturing());

        // 0x97 opens the capture.
        let events = parser.push(&hex("ac550003970198"));
        assert_eq!(events, vec![DeviceEvent::Voice(VoiceEvent::Started)]);
        assert!(parser.is_capturing());

        let events = parser.push(&hex(VOICE));
        let [DeviceEvent::Voice(VoiceEvent::Packet(p))] = events.as_slice() else {
            panic!("expected one packet, got {events:?}");
        };
        assert_eq!(p.payload.len(), 40, "every captured voice payload is 40 bytes");
        assert_eq!(p.payload[0], 0x4B, "Opus TOC — parsed, never decoded, in this crate");
        assert_eq!(p.index, 0);

        // Bare `parse` cannot do this and says so rather than guessing: the frame comes
        // back as a routing instruction.
        let frame = decode_device(&hex(VOICE)).unwrap();
        assert!(matches!(parse(&frame), DeviceEvent::VoiceFrame { cmd: 0x46, .. }));
    }

    /// A `0x46` with no preceding `0x97` recovers the capture rather than dropping the
    /// utterance. On this firmware the stream does not restart, so a dropped head is a
    /// dropped wake word.
    ///
    /// The rule itself lives in [`crate::voice`] and is tested there; this asserts the
    /// [`Parser`] seam actually routes through it, which is the property that keeps the
    /// rule from being written a second time.
    #[test]
    fn a_voice_chunk_with_no_start_packet_recovers_the_capture() {
        const VOICE: &str = "ac55002a464b410516aed2dea71b251b88ee731dbd78b1d8b1c98f\
                             d34a354f35796e124b53043ab2000000000077";
        let mut parser = Parser::new();
        let events = parser.push(&hex(VOICE));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], DeviceEvent::Voice(VoiceEvent::StartedRecovered));
        assert!(matches!(
            events[1],
            DeviceEvent::Voice(VoiceEvent::Packet(_))
        ));
        assert!(parser.is_capturing());

        // The second chunk is just a packet — recovery happens once.
        let events = parser.push(&hex(VOICE));
        assert_eq!(events.len(), 1);
    }

    /// `0x99` is never sent by this firmware, and when it is synthesised (by a test, or a
    /// future firmware) it must not END a capture that never began — that would flush an
    /// empty utterance into the recogniser. It is reported as ignored rather than dropped,
    /// so a router's coverage stays assertable.
    #[test]
    fn a_voice_end_with_no_capture_open_reports_ignored_rather_than_an_end() {
        let mut parser = Parser::new();
        let end = encode_device(op::VOICE_END, &[0x00]);
        assert_eq!(parser.push(&end), vec![DeviceEvent::Voice(VoiceEvent::Ignored)]);

        parser.push(&hex("ac550003970198"));
        assert_eq!(
            parser.push(&end),
            vec![DeviceEvent::Voice(VoiceEvent::Ended(EndCause::DeviceSignalled))]
        );
        assert!(!parser.is_capturing());
        assert_eq!(parser.push(&end), vec![DeviceEvent::Voice(VoiceEvent::Ignored)]);
    }

    /// The mic closes on an app WRITE, so the parser has to be told. Nothing inbound
    /// announces it: four captures totalling 14,636 voice frames contain no `0x99` and no
    /// gap, and a parser that waits to observe the close waits forever.
    #[test]
    fn only_an_app_side_close_ends_the_capture_from_the_parsers_point_of_view() {
        let mut parser = Parser::new();
        parser.push(&hex("ac550003970198"));
        assert!(parser.is_capturing());
        assert_eq!(
            parser.close_voice(EndCause::Interrupted),
            vec![DeviceEvent::Voice(VoiceEvent::Ended(EndCause::Interrupted))]
        );
        assert!(!parser.is_capturing());
        // Closing again says nothing happened rather than inventing a second end.
        assert_eq!(
            parser.close_voice(EndCause::Interrupted),
            vec![DeviceEvent::Voice(VoiceEvent::Ignored)]
        );

        // Reset clears the capture state as well as the frame buffer — a stale "capturing"
        // across a reconnect swallows the next session's recovered start.
        parser.push(&hex("ac550003970198"));
        parser.push(&[0xAC, 0x55, 0x00]);
        assert_eq!(parser.buffered(), &[0xAC, 0x55, 0x00]);
        parser.reset();
        assert!(!parser.is_capturing());
        assert!(parser.buffered().is_empty());
    }

    #[test]
    fn an_abandon_frame_drops_the_open_capture() {
        let mut parser = Parser::new();
        parser.push(&hex("ac550003970198"));
        let bytes = encode_device(op::ABANDON_VOICE, &[0x00]);
        assert_eq!(parser.push(&bytes), vec![DeviceEvent::VoiceAbandoned]);
        assert!(!parser.is_capturing());
    }

    // -- acks, unknowns, and the whole stream --------------------------------------------

    /// Setter echoes carry no state and arrive as acks with their payload intact. Every
    /// frame below is a real capture line — including `0x24`, whose echo is `00` where
    /// `0x23`'s is `01`, which is the kind of asymmetry a hand-written expectation gets
    /// wrong.
    #[test]
    fn captured_setter_echoes_arrive_as_typed_acks() {
        let cases: &[(&str, AppCommand, &[u8])] = &[
            ("ac550003220123", AppCommand::TakePhoto, &[0x01]),
            ("ac550003230124", AppCommand::StartVideo, &[0x01]),
            ("ac550003240024", AppCommand::StopVideo, &[0x00]),
            ("ac550003300131", AppCommand::SwitchMusic, &[0x01]),
            ("ac550003310031", AppCommand::PlayPause, &[0x00]),
            ("ac550003320133", AppCommand::Volume, &[0x01]),
            ("ac550003340135", AppCommand::VoiceRecording, &[0x01]),
            ("ac550003560056", AppCommand::InterruptVoice, &[0x00]),
            ("ac55000444300175", AppCommand::FileDownloadComplete, &[0x30, 0x01]),
            (
                "ac550008591a071b122812e1",
                AppCommand::SendPhoneTime,
                &[0x1A, 0x07, 0x1B, 0x12, 0x28, 0x12],
            ),
        ];
        for (h, command, data) in cases {
            assert_eq!(
                event(h),
                DeviceEvent::Ack {
                    command: *command,
                    data: data.to_vec(),
                },
                "{h}"
            );
        }
    }

    /// The `0x36` probe reply. It is an ACK that echoes the index it was given — and the
    /// payload is kept precisely because for this opcode the echoed byte is the entire
    /// result. No data ever follows it; the ack exists and the data path does not.
    #[test]
    fn the_image_pull_probe_reply_keeps_the_echoed_index() {
        for index in [0x00u8, 0x01, 0x02, 0x05] {
            let bytes = encode_device(AppCommand::PullImage as u8, &[index]);
            assert_eq!(
                parse(&decode_device(&bytes).unwrap()),
                DeviceEvent::Ack {
                    command: AppCommand::PullImage,
                    data: vec![index],
                }
            );
        }
        assert_eq!(AppCommand::PullImage.evidence(), Evidence::DeviceProbe);
    }

    /// A byte in neither table is reported with its payload, not dropped. That is what
    /// makes probing an unknown opcode possible without first enshrining it — and the
    /// `0xEE`/`0xDE` controls in the documented probe method are exactly this path.
    #[test]
    fn an_unknown_opcode_is_reported_with_its_bytes_rather_than_guessed() {
        for cmd in [0xEEu8, 0xDE, 0x03, 0x16, 0x35, 0x43, 0x50] {
            let bytes = encode_device(cmd, &[0x12, 0x34]);
            assert_eq!(
                parse(&decode_device(&bytes).unwrap()),
                DeviceEvent::Unrecognised {
                    cmd,
                    data: vec![0x12, 0x34],
                },
                "{cmd:#04x}"
            );
        }
    }

    /// Every opcode either decodes to a typed event or is explicitly an ack — nothing in
    /// the union falls into `Unrecognised`, which is what "one table, no thirteenth gap"
    /// means in practice.
    #[test]
    fn no_opcode_in_either_table_decodes_as_unrecognised() {
        for cmd in AppCommand::ALL
            .iter()
            .map(|c| c.code())
            .chain(DeviceUpload::ALL.iter().map(|u| u.code()))
        {
            // A generous payload so no decode fails for length; the point is the dispatch.
            let bytes = encode_device(cmd, &[0x30; 16]);
            let e = parse(&decode_device(&bytes).unwrap());
            assert!(
                !matches!(e, DeviceEvent::Unrecognised { .. }),
                "{cmd:#04x} fell through the dispatch: {e:?}"
            );
        }
    }

    /// The real inbound sequence of a connect, taken in capture order from capture `client_1`
    /// and fed to the parser as ONE coalesced blob. Everything a shell needs to bring up its
    /// UI — firmware, identity, battery, capabilities, every switch and all five gesture
    /// bindings — comes out of one call with no byte offsets anywhere.
    #[test]
    fn the_captured_connect_sequence_parses_end_to_end_from_one_blob() {
        const CONNECT: &[&str] = &[
            "ac550006950000040ba4",       // capability push, ~30 ms after link-up
            "ac550008591a071b140334e0",   // phone-time echo
            "ac550009550104080103010269", // versions
            "ac55000a645431000030333033af", // identity
            "ac550005173a300182",         // battery 100 %, charging
            "ac550003013132",             // ── 0x48 burst ──
            "ac5500040200b4b6",
            "ac550003043135",
            "ac550003063137",
            "ac550003073037",
            "ac550003083139",
            "ac55000309323b",
            "ac550003103444",
            "ac550003113344",
            "ac550003613091",
            "ac55000c450000000000000001000046", // action sync, music playing
            "ac550005690b0b0f8e",               // volumes
            "ac55000442001254",                 // 18 files waiting
        ];
        let blob: Vec<u8> = CONNECT.iter().flat_map(|h| hex(h)).collect();

        let mut parser = Parser::new();
        let (events, residual) = parser.push_detailed(&blob);
        assert_eq!(events.len(), CONNECT.len());
        assert_eq!(residual, Residual::Empty);
        assert!(!events.iter().any(|e| matches!(
            e,
            DeviceEvent::Malformed { .. } | DeviceEvent::Unrecognised { .. }
        )));

        let mut states = SwitchStates::new();
        for e in &events {
            states.ingest(e);
        }
        assert!(states.is_complete(), "the burst filled every field");
        assert_eq!(states.record_seconds(), Some(180));
        assert_eq!(states.gesture(GestureSlot::TripleTap), Some(GestureAction::NextTrack));

        assert!(events.contains(&DeviceEvent::Battery(BatteryReading {
            percent: 100,
            charging: true,
            source: BatterySource::ReadReply,
        })));
        assert!(events.contains(&DeviceEvent::Capabilities(Capabilities {
            raw: Capabilities::KNOWN_E09,
        })));
        assert!(events.contains(&DeviceEvent::Volumes(Volumes {
            system: 0x0B,
            media: 0x0B,
            call: 0x0F,
        })));
        assert!(events.contains(&DeviceEvent::MediaCount(MediaCount {
            count: 18,
            source: CountSource::Push,
        })));

        // The same blob split at every byte boundary yields the same events — the
        // deframer's job, asserted here because a shell only ever sees this seam.
        for cut in 1..blob.len() {
            let mut parser = Parser::new();
            let mut got = parser.push(&blob[..cut]);
            got.extend(parser.push(&blob[cut..]));
            assert_eq!(got, events, "split at {cut}");
        }
    }

    /// `AA15` interleaves `52 58` file bytes with control frames, so bytes that are not
    /// ours come BACK rather than being dropped — dropping them loses image data.
    #[test]
    fn file_stream_bytes_are_handed_back_instead_of_parsed() {
        let mut parser = Parser::new();
        let mut stream = hex("ac550003970198");
        stream.extend_from_slice(&[0x52, 0x58, 0x00, 0x07]);
        let (events, residual) = parser.push_detailed(&stream);
        assert_eq!(events, vec![DeviceEvent::Voice(VoiceEvent::Started)]);
        assert_eq!(residual, Residual::Foreign(vec![0x52, 0x58, 0x00, 0x07]));
    }
}
