//! The UniFFI surface for the glasses protocol.
//!
//! Thin translation over [`frame`](crate::frame), [`opcodes`](crate::opcodes),
//! [`parser`](crate::parser), [`reassembly`](crate::reassembly), [`voice`](crate::voice),
//! [`commands`](crate::commands) and [`gatt`](crate::gatt). Four rules hold across it:
//!
//! * **Every free function is prefixed.** UniFFI's namespace is flat, so every protocol
//!   function is `glasses_*`.
//! * **Nothing crosses as a future.** UniFFI's Swift bindings do not satisfy `Sendable` for
//!   async functions (mozilla/uniffi-rs#2274), so the whole boundary is synchronous and the
//!   client keeps its concurrency, its timers and its sockets on its own side.
//! * **No user-facing copy.** Every string that crosses is a protocol identity — a UUID, an
//!   SSID, a project code — never a word for a screen.
//! * **No durations.** The client owns the 1.2 s voice-idle gap, the 30 s open-mic cap and the
//!   ~2 s SSID wait. This crate can NAME a moment ([`GlassesParser::close_voice`]) and cannot
//!   measure one.
//!
//! ## Shape, and why the objects are what they are
//!
//! A client feeds bytes in per characteristic and publishes decoded state out. So the
//! stateful pieces cross as OBJECTS with methods:
//!
//! | Characteristic | Object | What the client does with the result |
//! |---|---|---|
//! | `AA14` control | [`GlassesParser`] | `push(bytes) -> [event]`, publish |
//! | `AA15` file/voice | [`GlassesParser`] then [`GlassesFileReassembler`] | route the parser's `Foreign` residual into the reassembler |
//!
//! [`GlassesParser`] folds every event into a [`SwitchStates`](crate::parser::SwitchStates) of
//! its own as it goes, so the `0x48` burst is already accumulated by the time the client asks
//! ([`GlassesParser::switch_states`]). That is deliberate: a client that keeps its own
//! accumulator is a client that can disagree about what "settings synced" means — and the two
//! reference implementations this was ported from each did, one reporting complete without any
//! of the five gesture slots and the other waiting for an aggregate `0x48` frame the firmware
//! never sends.

use std::sync::{Arc, Mutex};

use crate::commands;
use crate::frame::{self, Deframer, Frame, Residual};
use crate::gatt;
use crate::opcodes::{AppCommand, Evidence, GestureAction, GestureSlot, VolumeChannel};
use crate::parser::{
    self, DeviceAction, DeviceEvent, MalformedReason, Orientation, Parser, SwitchReport,
    SwitchStates, WifiService,
};
use crate::reassembly::{FileAbort, FileEvent, FileKind, FileReassembler};
use crate::voice::{EndCause, OpusBandwidth, OpusMode, OpusToc, VoiceEvent, VoiceStream};

// ---------------------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------------------

/// How strongly an opcode or a flag is evidenced. Crosses the boundary because a UI binding
/// built on a [`Self::ClientOnly`] value renders a state no device has ever reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesEvidence {
    Capture,
    DeviceProbe,
    ClientOnly,
}

impl From<Evidence> for FfiGlassesEvidence {
    fn from(e: Evidence) -> Self {
        match e {
            Evidence::Capture => FfiGlassesEvidence::Capture,
            Evidence::DeviceProbe => FfiGlassesEvidence::DeviceProbe,
            Evidence::ClientOnly => FfiGlassesEvidence::ClientOnly,
        }
    }
}

/// App → device opcode. The discriminant is NOT the wire byte on this side — use
/// [`glasses_command_code`] — because UniFFI enums do not carry Rust discriminants across.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesCommand {
    SetLed,
    SetRecordDuration,
    WearDetection,
    VoiceCommand,
    SetGestureSwipeForward,
    SetGestureSwipeBack,
    SetGestureSingleTap,
    SetGestureDoubleTap,
    SetGestureTripleTap,
    FactoryReset,
    GetBattery,
    TakePhoto,
    StartVideo,
    StopVideo,
    SwitchMusic,
    PlayPause,
    Volume,
    AnswerHangup,
    VoiceRecording,
    PullImage,
    PullThumbnailStatus,
    OpenWifiFiles,
    GetFileCount,
    FileDownloadComplete,
    GetDeviceStatus,
    GetSwitchStates,
    GetVersions,
    InterruptVoice,
    RetransmitVoice,
    SendPhoneTime,
    Reboot,
    Orientation,
    OfflineVoiceLang,
    EnterUpgrade,
    GetProjectName,
    SendIspVersion,
    OpenWifiLive,
    GetVolumes,
    SetVolume,
    GetVoiceDisableState,
    GetCapabilities,
}

impl From<AppCommand> for FfiGlassesCommand {
    fn from(c: AppCommand) -> Self {
        use AppCommand as A;
        use FfiGlassesCommand as F;
        match c {
            A::SetLed => F::SetLed,
            A::SetRecordDuration => F::SetRecordDuration,
            A::WearDetection => F::WearDetection,
            A::VoiceCommand => F::VoiceCommand,
            A::SetGestureSwipeForward => F::SetGestureSwipeForward,
            A::SetGestureSwipeBack => F::SetGestureSwipeBack,
            A::SetGestureSingleTap => F::SetGestureSingleTap,
            A::SetGestureDoubleTap => F::SetGestureDoubleTap,
            A::SetGestureTripleTap => F::SetGestureTripleTap,
            A::FactoryReset => F::FactoryReset,
            A::GetBattery => F::GetBattery,
            A::TakePhoto => F::TakePhoto,
            A::StartVideo => F::StartVideo,
            A::StopVideo => F::StopVideo,
            A::SwitchMusic => F::SwitchMusic,
            A::PlayPause => F::PlayPause,
            A::Volume => F::Volume,
            A::AnswerHangup => F::AnswerHangup,
            A::VoiceRecording => F::VoiceRecording,
            A::PullImage => F::PullImage,
            A::PullThumbnailStatus => F::PullThumbnailStatus,
            A::OpenWifiFiles => F::OpenWifiFiles,
            A::GetFileCount => F::GetFileCount,
            A::FileDownloadComplete => F::FileDownloadComplete,
            A::GetDeviceStatus => F::GetDeviceStatus,
            A::GetSwitchStates => F::GetSwitchStates,
            A::GetVersions => F::GetVersions,
            A::InterruptVoice => F::InterruptVoice,
            A::RetransmitVoice => F::RetransmitVoice,
            A::SendPhoneTime => F::SendPhoneTime,
            A::Reboot => F::Reboot,
            A::Orientation => F::Orientation,
            A::OfflineVoiceLang => F::OfflineVoiceLang,
            A::EnterUpgrade => F::EnterUpgrade,
            A::GetProjectName => F::GetProjectName,
            A::SendIspVersion => F::SendIspVersion,
            A::OpenWifiLive => F::OpenWifiLive,
            A::GetVolumes => F::GetVolumes,
            A::SetVolume => F::SetVolume,
            A::GetVoiceDisableState => F::GetVoiceDisableState,
            A::GetCapabilities => F::GetCapabilities,
        }
    }
}

impl From<FfiGlassesCommand> for AppCommand {
    fn from(c: FfiGlassesCommand) -> Self {
        use AppCommand as A;
        use FfiGlassesCommand as F;
        match c {
            F::SetLed => A::SetLed,
            F::SetRecordDuration => A::SetRecordDuration,
            F::WearDetection => A::WearDetection,
            F::VoiceCommand => A::VoiceCommand,
            F::SetGestureSwipeForward => A::SetGestureSwipeForward,
            F::SetGestureSwipeBack => A::SetGestureSwipeBack,
            F::SetGestureSingleTap => A::SetGestureSingleTap,
            F::SetGestureDoubleTap => A::SetGestureDoubleTap,
            F::SetGestureTripleTap => A::SetGestureTripleTap,
            F::FactoryReset => A::FactoryReset,
            F::GetBattery => A::GetBattery,
            F::TakePhoto => A::TakePhoto,
            F::StartVideo => A::StartVideo,
            F::StopVideo => A::StopVideo,
            F::SwitchMusic => A::SwitchMusic,
            F::PlayPause => A::PlayPause,
            F::Volume => A::Volume,
            F::AnswerHangup => A::AnswerHangup,
            F::VoiceRecording => A::VoiceRecording,
            F::PullImage => A::PullImage,
            F::PullThumbnailStatus => A::PullThumbnailStatus,
            F::OpenWifiFiles => A::OpenWifiFiles,
            F::GetFileCount => A::GetFileCount,
            F::FileDownloadComplete => A::FileDownloadComplete,
            F::GetDeviceStatus => A::GetDeviceStatus,
            F::GetSwitchStates => A::GetSwitchStates,
            F::GetVersions => A::GetVersions,
            F::InterruptVoice => A::InterruptVoice,
            F::RetransmitVoice => A::RetransmitVoice,
            F::SendPhoneTime => A::SendPhoneTime,
            F::Reboot => A::Reboot,
            F::Orientation => A::Orientation,
            F::OfflineVoiceLang => A::OfflineVoiceLang,
            F::EnterUpgrade => A::EnterUpgrade,
            F::GetProjectName => A::GetProjectName,
            F::SendIspVersion => A::SendIspVersion,
            F::OpenWifiLive => A::OpenWifiLive,
            F::GetVolumes => A::GetVolumes,
            F::SetVolume => A::SetVolume,
            F::GetVoiceDisableState => A::GetVoiceDisableState,
            F::GetCapabilities => A::GetCapabilities,
        }
    }
}

/// One touchpad gesture slot. Slot IDs are DEVICE-CONFIRMED; which physical gesture each is
/// remains INFERRED from the PDF's default table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesGestureSlot {
    SwipeForward,
    SwipeBack,
    SingleTap,
    DoubleTap,
    TripleTap,
}

impl From<GestureSlot> for FfiGlassesGestureSlot {
    fn from(s: GestureSlot) -> Self {
        match s {
            GestureSlot::SwipeForward => FfiGlassesGestureSlot::SwipeForward,
            GestureSlot::SwipeBack => FfiGlassesGestureSlot::SwipeBack,
            GestureSlot::SingleTap => FfiGlassesGestureSlot::SingleTap,
            GestureSlot::DoubleTap => FfiGlassesGestureSlot::DoubleTap,
            GestureSlot::TripleTap => FfiGlassesGestureSlot::TripleTap,
        }
    }
}

impl From<FfiGlassesGestureSlot> for GestureSlot {
    fn from(s: FfiGlassesGestureSlot) -> Self {
        match s {
            FfiGlassesGestureSlot::SwipeForward => GestureSlot::SwipeForward,
            FfiGlassesGestureSlot::SwipeBack => GestureSlot::SwipeBack,
            FfiGlassesGestureSlot::SingleTap => GestureSlot::SingleTap,
            FfiGlassesGestureSlot::DoubleTap => GestureSlot::DoubleTap,
            FfiGlassesGestureSlot::TripleTap => GestureSlot::TripleTap,
        }
    }
}

/// What a gesture slot is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesGestureAction {
    VolumeDown,
    VolumeUp,
    PlayPause,
    NextTrack,
    PreviousTrack,
}

impl From<GestureAction> for FfiGlassesGestureAction {
    fn from(a: GestureAction) -> Self {
        match a {
            GestureAction::VolumeDown => FfiGlassesGestureAction::VolumeDown,
            GestureAction::VolumeUp => FfiGlassesGestureAction::VolumeUp,
            GestureAction::PlayPause => FfiGlassesGestureAction::PlayPause,
            GestureAction::NextTrack => FfiGlassesGestureAction::NextTrack,
            GestureAction::PreviousTrack => FfiGlassesGestureAction::PreviousTrack,
        }
    }
}

impl From<FfiGlassesGestureAction> for GestureAction {
    fn from(a: FfiGlassesGestureAction) -> Self {
        match a {
            FfiGlassesGestureAction::VolumeDown => GestureAction::VolumeDown,
            FfiGlassesGestureAction::VolumeUp => GestureAction::VolumeUp,
            FfiGlassesGestureAction::PlayPause => GestureAction::PlayPause,
            FfiGlassesGestureAction::NextTrack => GestureAction::NextTrack,
            FfiGlassesGestureAction::PreviousTrack => GestureAction::PreviousTrack,
        }
    }
}

/// The three independently-addressable output levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesVolumeChannel {
    System,
    Media,
    Call,
}

impl From<FfiGlassesVolumeChannel> for VolumeChannel {
    fn from(c: FfiGlassesVolumeChannel) -> Self {
        match c {
            FfiGlassesVolumeChannel::System => VolumeChannel::System,
            FfiGlassesVolumeChannel::Media => VolumeChannel::Media,
            FfiGlassesVolumeChannel::Call => VolumeChannel::Call,
        }
    }
}

impl From<VolumeChannel> for FfiGlassesVolumeChannel {
    fn from(c: VolumeChannel) -> Self {
        match c {
            VolumeChannel::System => FfiGlassesVolumeChannel::System,
            VolumeChannel::Media => FfiGlassesVolumeChannel::Media,
            VolumeChannel::Call => FfiGlassesVolumeChannel::Call,
        }
    }
}

/// Indicator-LED brightness. There is no "off".
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesLedLevel {
    Low,
    Mid,
    High,
}

impl From<FfiGlassesLedLevel> for commands::LedLevel {
    fn from(l: FfiGlassesLedLevel) -> Self {
        match l {
            FfiGlassesLedLevel::Low => commands::LedLevel::Low,
            FfiGlassesLedLevel::Mid => commands::LedLevel::Mid,
            FfiGlassesLedLevel::High => commands::LedLevel::High,
        }
    }
}

impl From<commands::LedLevel> for FfiGlassesLedLevel {
    fn from(l: commands::LedLevel) -> Self {
        match l {
            commands::LedLevel::Low => FfiGlassesLedLevel::Low,
            commands::LedLevel::Mid => FfiGlassesLedLevel::Mid,
            commands::LedLevel::High => FfiGlassesLedLevel::High,
        }
    }
}

/// Capture orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesOrientation {
    Portrait,
    Landscape,
}

impl From<Orientation> for FfiGlassesOrientation {
    fn from(o: Orientation) -> Self {
        match o {
            Orientation::Portrait => FfiGlassesOrientation::Portrait,
            Orientation::Landscape => FfiGlassesOrientation::Landscape,
        }
    }
}

impl From<FfiGlassesOrientation> for Orientation {
    fn from(o: FfiGlassesOrientation) -> Self {
        match o {
            FfiGlassesOrientation::Portrait => Orientation::Portrait,
            FfiGlassesOrientation::Landscape => Orientation::Landscape,
        }
    }
}

/// Which of the two Wi-Fi modes. Same SoftAP and same `0x44` teardown; the device serves
/// something different behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesWifiService {
    /// `0x39` — the JSON file API. The gallery path.
    Files,
    /// `0x67` — the RTSP live stream. Missing from the reference Android implementation entirely.
    Live,
}

impl From<WifiService> for FfiGlassesWifiService {
    fn from(s: WifiService) -> Self {
        match s {
            WifiService::Files => FfiGlassesWifiService::Files,
            WifiService::Live => FfiGlassesWifiService::Live,
        }
    }
}

impl From<FfiGlassesWifiService> for WifiService {
    fn from(s: FfiGlassesWifiService) -> Self {
        match s {
            FfiGlassesWifiService::Files => WifiService::Files,
            FfiGlassesWifiService::Live => WifiService::Live,
        }
    }
}

/// One flag of the `0x45` action-sync frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesDeviceAction {
    TakePhoto,
    AudioRecord,
    VideoRecord,
    VolumeUp,
    VolumeDown,
    NodHead,
    ShakeHead,
    PlayPause,
    WearingDetection,
    /// Index 9. Goes 1 while a Wi-Fi session is open. **NOT "Wi-Fi active"** — the vendor calls
    /// it file-import mode, and the reference iOS implementation publishes this bit as `wifiActive`, which is the
    /// drift this name closes.
    ImportingMode,
}

impl From<DeviceAction> for FfiGlassesDeviceAction {
    fn from(a: DeviceAction) -> Self {
        match a {
            DeviceAction::TakePhoto => FfiGlassesDeviceAction::TakePhoto,
            DeviceAction::AudioRecord => FfiGlassesDeviceAction::AudioRecord,
            DeviceAction::VideoRecord => FfiGlassesDeviceAction::VideoRecord,
            DeviceAction::VolumeUp => FfiGlassesDeviceAction::VolumeUp,
            DeviceAction::VolumeDown => FfiGlassesDeviceAction::VolumeDown,
            DeviceAction::NodHead => FfiGlassesDeviceAction::NodHead,
            DeviceAction::ShakeHead => FfiGlassesDeviceAction::ShakeHead,
            DeviceAction::PlayPause => FfiGlassesDeviceAction::PlayPause,
            DeviceAction::WearingDetection => FfiGlassesDeviceAction::WearingDetection,
            DeviceAction::ImportingMode => FfiGlassesDeviceAction::ImportingMode,
        }
    }
}

impl From<FfiGlassesDeviceAction> for DeviceAction {
    fn from(a: FfiGlassesDeviceAction) -> Self {
        match a {
            FfiGlassesDeviceAction::TakePhoto => DeviceAction::TakePhoto,
            FfiGlassesDeviceAction::AudioRecord => DeviceAction::AudioRecord,
            FfiGlassesDeviceAction::VideoRecord => DeviceAction::VideoRecord,
            FfiGlassesDeviceAction::VolumeUp => DeviceAction::VolumeUp,
            FfiGlassesDeviceAction::VolumeDown => DeviceAction::VolumeDown,
            FfiGlassesDeviceAction::NodHead => DeviceAction::NodHead,
            FfiGlassesDeviceAction::ShakeHead => DeviceAction::ShakeHead,
            FfiGlassesDeviceAction::PlayPause => DeviceAction::PlayPause,
            FfiGlassesDeviceAction::WearingDetection => DeviceAction::WearingDetection,
            FfiGlassesDeviceAction::ImportingMode => DeviceAction::ImportingMode,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Frame codec
// ---------------------------------------------------------------------------------------

/// One decoded frame: the opcode byte and its payload, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesFrame {
    pub cmd: u8,
    pub data: Vec<u8>,
}

impl From<&Frame> for FfiGlassesFrame {
    fn from(f: &Frame) -> Self {
        Self {
            cmd: f.cmd,
            data: f.data.clone(),
        }
    }
}

/// Why a byte sequence is not a frame.
///
/// `Truncated` and `BadLength` stay distinct across the boundary because the deframer WAITS on
/// the first and RESYNCS on the second — collapsing them is how the reference Android implementation wedged its
/// control channel on a corrupt length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesDecodeError {
    TooShort,
    BadHeader,
    BadLength,
    Truncated,
    ChecksumMismatch,
}

impl From<frame::DecodeError> for FfiGlassesDecodeError {
    fn from(e: frame::DecodeError) -> Self {
        match e {
            frame::DecodeError::TooShort => FfiGlassesDecodeError::TooShort,
            frame::DecodeError::BadHeader => FfiGlassesDecodeError::BadHeader,
            frame::DecodeError::BadLength => FfiGlassesDecodeError::BadLength,
            frame::DecodeError::Truncated => FfiGlassesDecodeError::Truncated,
            frame::DecodeError::ChecksumMismatch => FfiGlassesDecodeError::ChecksumMismatch,
        }
    }
}

/// Decode outcome — a total value, never a thrown error. Same reasoning as the FitCloud
/// decoder: neither side then depends on UniFFI's error lifting to switch on the case.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesDecoded {
    Ok { frame: FfiGlassesFrame },
    Err { reason: FfiGlassesDecodeError },
}

fn decoded(r: Result<Frame, frame::DecodeError>) -> FfiGlassesDecoded {
    match r {
        Ok(f) => FfiGlassesDecoded::Ok {
            frame: FfiGlassesFrame::from(&f),
        },
        Err(e) => FfiGlassesDecoded::Err { reason: e.into() },
    }
}

/// `(cmd + Σ data) & 0xFF` — the frame checksum. Header and length are outside it.
#[uniffi::export]
pub fn glasses_checksum(cmd: u8, data: Vec<u8>) -> u8 {
    frame::checksum(cmd, &data)
}

/// Build an app → device frame with a RAW opcode byte.
///
/// Raw so an unrecognised opcode can be probed without first being enshrined — that is how
/// `0x36` was characterised. An empty payload becomes the protocol-mandated `0x00` filler
/// (§2.1.1), which every captured read carries.
#[uniffi::export]
pub fn glasses_encode(cmd: u8, data: Vec<u8>) -> Vec<u8> {
    frame::encode(cmd, &data)
}

/// [`glasses_encode`] with a typed opcode.
#[uniffi::export]
pub fn glasses_encode_command(cmd: FfiGlassesCommand, data: Vec<u8>) -> Vec<u8> {
    frame::encode_command(cmd.into(), &data)
}

/// Build a device → app frame. This crate never plays device on a radio; this exists so tests,
/// fixtures and replay tooling can construct inbound bytes.
#[uniffi::export]
pub fn glasses_encode_device(cmd: u8, data: Vec<u8>) -> Vec<u8> {
    frame::encode_device(cmd, &data)
}

/// Decode exactly one device → app (`AC 55`) frame from the front of `bytes`. Trailing bytes
/// are ignored — a caller holding a coalesced buffer wants [`GlassesParser`] instead.
#[uniffi::export]
pub fn glasses_decode_device(bytes: Vec<u8>) -> FfiGlassesDecoded {
    decoded(frame::decode_device(&bytes))
}

/// Decode exactly one app → device (`AB 55`) frame. For reading back our own output and for
/// parsing the outbound side of a capture.
#[uniffi::export]
pub fn glasses_decode_app(bytes: Vec<u8>) -> FfiGlassesDecoded {
    decoded(frame::decode_app(&bytes))
}

/// The 4-byte envelope overhead: 2 magic + 2 length.
#[uniffi::export]
pub fn glasses_frame_overhead() -> u32 {
    frame::FRAME_OVERHEAD as u32
}

/// The largest length field the deframer will wait on. Anything larger is corruption and
/// triggers a resync rather than an indefinite wait.
#[uniffi::export]
pub fn glasses_max_length_field() -> u32 {
    frame::MAX_LENGTH_FIELD as u32
}

// ---------------------------------------------------------------------------------------
// Opcode tables
// ---------------------------------------------------------------------------------------

/// The wire byte for an opcode.
#[uniffi::export]
pub fn glasses_command_code(cmd: FfiGlassesCommand) -> u8 {
    AppCommand::from(cmd).code()
}

/// Classify an outbound byte. `None` is a real answer — the encoder takes raw bytes precisely
/// so an unknown opcode can be probed.
#[uniffi::export]
pub fn glasses_command_from_code(code: u8) -> Option<FfiGlassesCommand> {
    AppCommand::from_code(code).map(Into::into)
}

/// What backs this opcode.
#[uniffi::export]
pub fn glasses_command_evidence(cmd: FfiGlassesCommand) -> FfiGlassesEvidence {
    AppCommand::from(cmd).evidence().into()
}

/// Every app → device opcode, ascending by wire byte.
#[uniffi::export]
pub fn glasses_all_commands() -> Vec<FfiGlassesCommand> {
    AppCommand::ALL.into_iter().map(Into::into).collect()
}

/// The exact order of the `0x48` switch-state burst, as ten separate frames.
///
/// **There is no `0x48` reply.** A shell waiting for one waits forever; these are the ten
/// opcodes the answer actually arrives under.
#[uniffi::export]
pub fn glasses_switch_state_burst_order() -> Vec<FfiGlassesCommand> {
    crate::opcodes::SWITCH_STATE_BURST_ORDER
        .into_iter()
        .map(Into::into)
        .collect()
}

/// An opcode the vendor SDK names but which neither client implements and no capture has
/// produced.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesUnimplementedOpcode {
    pub code: u8,
    /// The vendor's own identifier for it — a grep key, never user-facing copy.
    pub vendor_name: String,
}

/// The five vendor-named opcodes nobody has evidence for. Recorded so a future session does not
/// re-derive them from a decompiled jump table and start sending bytes at hardware.
#[uniffi::export]
pub fn glasses_vendor_named_unimplemented() -> Vec<FfiGlassesUnimplementedOpcode> {
    crate::opcodes::VENDOR_NAMED_UNIMPLEMENTED
        .iter()
        .map(|(code, name)| FfiGlassesUnimplementedOpcode {
            code: *code,
            vendor_name: (*name).to_string(),
        })
        .collect()
}

/// All five gesture slots, in the order the burst emits them.
#[uniffi::export]
pub fn glasses_gesture_slots() -> Vec<FfiGlassesGestureSlot> {
    GestureSlot::ALL.into_iter().map(Into::into).collect()
}

/// The setter opcode that carries a slot — the same byte, stated rather than assumed.
#[uniffi::export]
pub fn glasses_gesture_slot_command(slot: FfiGlassesGestureSlot) -> FfiGlassesCommand {
    GestureSlot::from(slot).command().into()
}

/// Decode a slot frame's value byte. Accepts the ASCII digit every captured firmware sends and
/// a raw `0..=4`; anything else is `None` rather than a silent clamp onto "volume down".
#[uniffi::export]
pub fn glasses_gesture_action_from_value(byte: u8) -> Option<FfiGlassesGestureAction> {
    GestureAction::from_value(byte).map(Into::into)
}

/// Every binding a slot can take, in ordinal order — the list a settings picker offers.
///
/// Exported rather than left to each shell because a hardcoded picker order is a
/// presentation-layer copy of a wire table, and the reference implementations already proved that copy drifts.
#[uniffi::export]
pub fn glasses_gesture_actions() -> Vec<FfiGlassesGestureAction> {
    GestureAction::ALL.into_iter().map(Into::into).collect()
}

/// The value byte a slot frame carries for this binding — the ASCII digit, i.e. what
/// [`glasses_set_gesture`] puts on the wire and what
/// [`FfiGlassesSwitchStates::gestures_raw`] reports back.
///
/// The exact inverse of [`glasses_gesture_action_from_value`], so a shell comparing a reported
/// raw byte against a candidate binding never has to add `0x30` itself.
#[uniffi::export]
pub fn glasses_gesture_action_value(action: FfiGlassesGestureAction) -> u8 {
    GestureAction::from(action).ascii()
}

/// The three LED brightnesses, dim → bright.
#[uniffi::export]
pub fn glasses_led_levels() -> Vec<FfiGlassesLedLevel> {
    commands::LedLevel::ALL.into_iter().map(Into::into).collect()
}

/// Map a REPORTED LED level back to the enum [`glasses_set_led`] takes.
///
/// The argument is the `0..=2` level [`FfiGlassesSwitchStates::led`] carries, NOT the ASCII
/// byte off the wire — the parser has already subtracted `'0'` by then. Out of range is `None`
/// rather than a clamp: on iOS the reported level round-trips through `@AppStorage` back to the
/// hardware, so clamping would push a value the device never reported back at it as if it had.
#[uniffi::export]
pub fn glasses_led_level_from_value(level: u8) -> Option<FfiGlassesLedLevel> {
    commands::LedLevel::from_level(level).map(Into::into)
}

/// The `0..=2` level this brightness reports as. The inverse of
/// [`glasses_led_level_from_value`].
#[uniffi::export]
pub fn glasses_led_level_value(level: FfiGlassesLedLevel) -> u8 {
    commands::LedLevel::from(level).ascii() - b'0'
}

/// The three output channels, in the order a `0x69` reply lists them — so a shell reading that
/// reply indexes it by this list instead of re-deriving the order.
#[uniffi::export]
pub fn glasses_volume_channels() -> Vec<FfiGlassesVolumeChannel> {
    VolumeChannel::ALL.into_iter().map(Into::into).collect()
}

/// The channel byte `0x70` carries.
#[uniffi::export]
pub fn glasses_volume_channel_code(channel: FfiGlassesVolumeChannel) -> u8 {
    VolumeChannel::from(channel).code()
}

/// Every action flag of the `0x45` frame, in payload-index order.
#[uniffi::export]
pub fn glasses_device_actions() -> Vec<FfiGlassesDeviceAction> {
    DeviceAction::ALL.into_iter().map(Into::into).collect()
}

/// What backs one action flag.
///
/// Three of the ten — nod, shake, wearing — have never been seen SET on any device across all
/// 85 captured action-sync frames. A UI binding on one of those renders a state the hardware
/// has never reported.
#[uniffi::export]
pub fn glasses_device_action_evidence(action: FfiGlassesDeviceAction) -> FfiGlassesEvidence {
    DeviceAction::from(action).evidence().into()
}

// ---------------------------------------------------------------------------------------
// GATT
// ---------------------------------------------------------------------------------------

/// The GATT topology, so each shell converts to its own UUID type once at the edge instead of
/// carrying a copy of the table.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesGatt {
    pub service: String,
    pub write: String,
    pub control_notify: String,
    pub file_notify: String,
    /// Both notify characteristics, control FIRST — a failed control subscribe is not
    /// survivable (it carries the voice stream), a failed file subscribe only costs transfers.
    pub notify_all: Vec<String>,
    /// `true` ⇒ ATT Write **Request**. DEVICE-CONFIRMED in all eight captures: 0 Write
    /// Commands, and every Write Request answered. Android hardcodes the other one today.
    pub write_with_response: bool,
}

#[uniffi::export]
pub fn glasses_gatt() -> FfiGlassesGatt {
    FfiGlassesGatt {
        service: gatt::SERVICE.into(),
        write: gatt::WRITE.into(),
        control_notify: gatt::CONTROL_NOTIFY.into(),
        file_notify: gatt::FILE_NOTIFY.into(),
        notify_all: gatt::NOTIFY_ALL.iter().map(|s| s.to_string()).collect(),
        write_with_response: gatt::WRITE_WITH_RESPONSE,
    }
}

/// Can this characteristic set be driven? Needs the write characteristic plus ≥1 notify.
/// Matching is case-insensitive substring, so a 16-bit and a 128-bit spelling both resolve.
#[uniffi::export]
pub fn glasses_is_drivable(characteristic_uuids: Vec<String>) -> bool {
    gatt::is_drivable(characteristic_uuids.iter().map(String::as_str))
}

// ---------------------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------------------
//
// The payload conventions, once. Settings booleans are ASCII `'1'`/`'0'`; media booleans are
// raw `0x01`/`0x00`. See `crate::commands` for the table and the per-builder evidence.

/// `0x22` — take a photo; `for_ai` also pulls the full-resolution still.
#[uniffi::export]
pub fn glasses_take_photo(for_ai: bool) -> Vec<u8> {
    commands::take_photo(for_ai)
}

/// `0x23` — start a video recording.
#[uniffi::export]
pub fn glasses_start_video() -> Vec<u8> {
    commands::start_video()
}

/// `0x24` — stop it.
#[uniffi::export]
pub fn glasses_stop_video() -> Vec<u8> {
    commands::stop_video()
}

/// `0x34` — start / stop an on-device audio recording.
#[uniffi::export]
pub fn glasses_voice_recording(start: bool) -> Vec<u8> {
    commands::voice_recording(start)
}

/// `0x40` — how many photos/videos the device holds. The reply is a `0x42`.
#[uniffi::export]
pub fn glasses_get_file_count() -> Vec<u8> {
    commands::get_file_count()
}

/// `0x39` (files) / `0x67` (live) — raise the SoftAP. The SSID follows in a `0x25` about 2.5 s
/// later; the passphrase is fixed and not on the wire ([`glasses_wifi_passphrase`]).
#[uniffi::export]
pub fn glasses_open_wifi(service: FfiGlassesWifiService, p2p: bool) -> Vec<u8> {
    commands::open_wifi(service.into(), p2p)
}

/// `0x44 30 00` — everything transferred, clear the thumbnail count.
#[uniffi::export]
pub fn glasses_file_download_complete() -> Vec<u8> {
    commands::file_download_complete()
}

/// `0x44 31 <n>` — partial: the device subtracts `downloaded` and re-reports the remainder.
#[uniffi::export]
pub fn glasses_file_download_partial(downloaded: u8) -> Vec<u8> {
    commands::file_download_partial(downloaded)
}

/// `0x44 30 01` — power the ISP off WITHOUT touching the count. **Send this on every Wi-Fi
/// session exit**, including abort and error; otherwise the glasses tone in SoftAP-waiting mode
/// indefinitely.
#[uniffi::export]
pub fn glasses_power_off_isp() -> Vec<u8> {
    commands::power_off_isp()
}

/// `0x30` — previous / next track.
#[uniffi::export]
pub fn glasses_switch_music(next: bool) -> Vec<u8> {
    commands::switch_music(next)
}

/// `0x31` — play / pause.
#[uniffi::export]
pub fn glasses_play_pause(play: bool) -> Vec<u8> {
    commands::play_pause(play)
}

/// `0x32` — volume STEP up/down. Distinct from [`glasses_set_volume`], which sets one channel's
/// absolute level.
#[uniffi::export]
pub fn glasses_volume_step(up: bool) -> Vec<u8> {
    commands::volume_step(up)
}

/// `0x33` — answer / hang up. CLIENT-ONLY opcode.
#[uniffi::export]
pub fn glasses_answer_hangup(answer: bool) -> Vec<u8> {
    commands::answer_hangup(answer)
}

/// `0x70` — set ONE output level. `level` is carried verbatim on an unpinned scale.
#[uniffi::export]
pub fn glasses_set_volume(channel: FfiGlassesVolumeChannel, level: u8) -> Vec<u8> {
    commands::set_volume(channel.into(), level)
}

/// `0x69` — read all three levels back, in `[system, media, call]` order.
#[uniffi::export]
pub fn glasses_get_volumes() -> Vec<u8> {
    commands::get_volumes()
}

/// `0x56` — **the only thing that closes the device mic.** No capture contains a `0x99` or a
/// gap, so a session waiting for the device to close its own microphone waits forever.
#[uniffi::export]
pub fn glasses_interrupt_voice() -> Vec<u8> {
    commands::interrupt_voice()
}

/// `0x57` — request a retransmit. CLIENT-ONLY.
#[uniffi::export]
pub fn glasses_retransmit_voice() -> Vec<u8> {
    commands::retransmit_voice()
}

/// `0x71` — read which voice features are DISABLED. `0x00` back means the wake word is live.
#[uniffi::export]
pub fn glasses_get_voice_disable_state() -> Vec<u8> {
    commands::get_voice_disable_state()
}

/// `0x01` — indicator-LED brightness.
#[uniffi::export]
pub fn glasses_set_led(level: FfiGlassesLedLevel) -> Vec<u8> {
    commands::set_led(level.into())
}

/// `0x02` — record duration in seconds, 2-byte BIG-endian.
#[uniffi::export]
pub fn glasses_set_record_duration(seconds: u16) -> Vec<u8> {
    commands::set_record_duration(seconds)
}

/// `0x04` — wear detection on/off.
#[uniffi::export]
pub fn glasses_set_wear_detection(on: bool) -> Vec<u8> {
    commands::set_wear_detection(on)
}

/// `0x06` — voice command / wake word on/off.
#[uniffi::export]
pub fn glasses_set_voice_command(on: bool) -> Vec<u8> {
    commands::set_voice_command(on)
}

/// `0x61` — capture orientation.
#[uniffi::export]
pub fn glasses_set_orientation(orientation: FfiGlassesOrientation) -> Vec<u8> {
    commands::set_orientation(orientation.into())
}

/// `0x62` — offline voice language. CLIENT-ONLY; never reported in a burst.
#[uniffi::export]
pub fn glasses_set_offline_voice_language(english: bool) -> Vec<u8> {
    commands::set_offline_voice_language(english)
}

/// `0x07`–`0x11` — bind one touchpad gesture slot. **Neither shipping client can send this.**
#[uniffi::export]
pub fn glasses_set_gesture(
    slot: FfiGlassesGestureSlot,
    action: FfiGlassesGestureAction,
) -> Vec<u8> {
    commands::set_gesture(slot.into(), action.into())
}

/// `0x59` — push the phone's wall clock, `YY MM DD HH MM SS` in **plain hex, not BCD**.
///
/// Components rather than an instant: this core reads no clock and no zone database, so the
/// shell's zone choice stays visible at the call site.
#[uniffi::export]
pub fn glasses_send_phone_time(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Vec<u8> {
    commands::send_phone_time(year, month, day, hour, minute, second)
}

/// `0x48` — read the switch states. The reply is the TEN-FRAME BURST, never a `0x48` frame.
#[uniffi::export]
pub fn glasses_get_switch_states() -> Vec<u8> {
    commands::get_switch_states()
}

/// `0x45` — read the ten action-sync flags.
#[uniffi::export]
pub fn glasses_get_device_status() -> Vec<u8> {
    commands::get_device_status()
}

/// `0x17` — battery.
#[uniffi::export]
pub fn glasses_get_battery() -> Vec<u8> {
    commands::get_battery()
}

/// `0x55` — firmware versions and hardware revision.
#[uniffi::export]
pub fn glasses_get_versions() -> Vec<u8> {
    commands::get_versions()
}

/// `0x64` — project and customer codes.
#[uniffi::export]
pub fn glasses_get_project_name() -> Vec<u8> {
    commands::get_project_name()
}

/// `0x95` — the capability word. Also arrives unsolicited ~30 ms after every link-up.
#[uniffi::export]
pub fn glasses_get_capabilities() -> Vec<u8> {
    commands::get_capabilities()
}

/// `0x14` — factory reset. CLIENT-ONLY.
#[uniffi::export]
pub fn glasses_factory_reset() -> Vec<u8> {
    commands::factory_reset()
}

/// `0x60` — reboot. CLIENT-ONLY.
#[uniffi::export]
pub fn glasses_reboot() -> Vec<u8> {
    commands::reboot()
}

/// `0x63` — enter upgrade mode.
#[uniffi::export]
pub fn glasses_enter_upgrade(p2p: bool) -> Vec<u8> {
    commands::enter_upgrade(p2p)
}

/// `0x65` — report the just-flashed ISP version. CLIENT-ONLY.
#[uniffi::export]
pub fn glasses_send_isp_version(major: u8, minor: u8, patch: u8) -> Vec<u8> {
    commands::send_isp_version(major, minor, patch)
}

/// The factory-fixed SoftAP passphrase, the same for `0x39` and `0x67`.
///
/// It is NOT on the wire — it lives here so the shell that joins the AP does not carry a magic
/// string of its own.
#[uniffi::export]
pub fn glasses_wifi_passphrase() -> String {
    parser::WifiCredentials::PASSPHRASE.to_string()
}

// ---------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------

/// Which frame a battery reading arrived in. Same quantity, two encodings, opposite field
/// order — reading one as the other yields a plausible wrong percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesBatterySource {
    /// `0x17` reply: `<tens><ones><charging>`, the digits ASCII.
    ReadReply,
    /// `0x53` push: `<charging><percent>`, percent a RAW byte.
    Push,
}

/// Where a media count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesCountSource {
    /// `0x42`. DEVICE-CONFIRMED.
    Push,
    /// `0x40` echoed back. INFERRED — no capture contains an inbound `0x40`.
    ReadReply,
}

/// Firmware versions from `0x55`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesVersions {
    pub bt_major: u8,
    pub bt_minor: u8,
    pub bt_patch: u8,
    pub isp_major: u8,
    pub isp_minor: u8,
    pub isp_patch: u8,
    pub hardware: u8,
}

/// One field of the `0x48` burst, or the echo of the setter that changed it. The two are
/// indistinguishable on the wire and mean the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesSwitchReport {
    /// `0x01` — LED brightness, `0..=2`.
    Led { level: u8 },
    /// `0x02` — record duration, seconds. The one burst value that is not an ASCII digit.
    RecordSeconds { seconds: u16 },
    /// `0x04`.
    WearDetection { on: bool },
    /// `0x06` — the wake word.
    VoiceCommand { on: bool },
    /// `0x61`.
    Orientation { orientation: FfiGlassesOrientation },
}

impl From<SwitchReport> for FfiGlassesSwitchReport {
    fn from(r: SwitchReport) -> Self {
        match r {
            SwitchReport::Led(level) => FfiGlassesSwitchReport::Led { level },
            SwitchReport::RecordSeconds(seconds) => {
                FfiGlassesSwitchReport::RecordSeconds { seconds }
            }
            SwitchReport::WearDetection(on) => FfiGlassesSwitchReport::WearDetection { on },
            SwitchReport::VoiceCommand(on) => FfiGlassesSwitchReport::VoiceCommand { on },
            SwitchReport::Orientation(o) => FfiGlassesSwitchReport::Orientation {
                orientation: o.into(),
            },
        }
    }
}

/// Why a frame could not be decoded into its typed field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesMalformedReason {
    /// Payload shorter than the layout needs; `need` is the minimum.
    TooShort { need: u32 },
    /// The layout holds but a value is out of its legal range — a battery over 100 %, an LED
    /// level above 2. Reported rather than clamped: a clamp puts a plausible wrong number on
    /// the user's screen.
    OutOfRange,
    /// A text field is not ASCII, or is empty once padding is gone.
    NotText,
}

impl From<MalformedReason> for FfiGlassesMalformedReason {
    fn from(r: MalformedReason) -> Self {
        match r {
            MalformedReason::TooShort { need } => {
                FfiGlassesMalformedReason::TooShort { need: need as u32 }
            }
            MalformedReason::OutOfRange => FfiGlassesMalformedReason::OutOfRange,
            MalformedReason::NotText => FfiGlassesMalformedReason::NotText,
        }
    }
}

/// Opus operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesOpusMode {
    Silk,
    Hybrid,
    Celt,
}

/// Opus audio bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesOpusBandwidth {
    Narrow,
    Medium,
    Wide,
    SuperWide,
    Full,
}

/// One Opus packet's self-describing header — packet FRAMING, not the codec. The range decoder
/// that turns the rest into samples stays native.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesOpusToc {
    pub config: u8,
    pub stereo: bool,
    pub code: u8,
    pub frames: u8,
    pub vbr: bool,
    pub padding: bool,
    pub mode: FfiGlassesOpusMode,
    pub bandwidth: FfiGlassesOpusBandwidth,
    /// The rate to request from the native decoder so it does not resample.
    pub nominal_rate_hz: u32,
    pub frame_duration_us: u32,
    /// `None` for a frame count of zero, which is malformed.
    pub total_duration_us: Option<u32>,
    pub channels: u8,
}

impl From<OpusToc> for FfiGlassesOpusToc {
    fn from(t: OpusToc) -> Self {
        Self {
            config: t.config,
            stereo: t.stereo,
            code: t.code,
            frames: t.frames,
            vbr: t.vbr,
            padding: t.padding,
            mode: match t.mode() {
                OpusMode::Silk => FfiGlassesOpusMode::Silk,
                OpusMode::Hybrid => FfiGlassesOpusMode::Hybrid,
                OpusMode::Celt => FfiGlassesOpusMode::Celt,
            },
            bandwidth: match t.bandwidth() {
                OpusBandwidth::Narrow => FfiGlassesOpusBandwidth::Narrow,
                OpusBandwidth::Medium => FfiGlassesOpusBandwidth::Medium,
                OpusBandwidth::Wide => FfiGlassesOpusBandwidth::Wide,
                OpusBandwidth::SuperWide => FfiGlassesOpusBandwidth::SuperWide,
                OpusBandwidth::Full => FfiGlassesOpusBandwidth::Full,
            },
            nominal_rate_hz: t.bandwidth().nominal_rate_hz(),
            frame_duration_us: t.frame_duration_us(),
            total_duration_us: t.total_duration_us(),
            channels: t.channels(),
        }
    }
}

/// One `0x46` payload: a complete Opus packet, ready for the native decoder.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesVoicePacket {
    /// The packet bytes, verbatim.
    pub payload: Vec<u8>,
    /// Its parsed header, or `None` if malformed. Malformed packets are still DELIVERED — a
    /// decoder that can make something of one should get the chance.
    pub toc: Option<FfiGlassesOpusToc>,
    /// Ordinal within the current capture, from 0. Distinguishes "the utterance had no audio"
    /// from "the utterance never opened", which both reference implementations conflate.
    pub index: u64,
}

/// Why a capture ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesEndCause {
    /// A `0x99` arrived. **Never observed** — a session that waits for this hangs.
    DeviceSignalled,
    /// The app wrote `0x56`. The only close any capture actually contains.
    Interrupted,
    /// The shell's inter-frame idle timer expired. The DURATION is the shell's.
    IdleTimeout,
    /// The link dropped mid-capture.
    Disconnected,
}

impl From<FfiGlassesEndCause> for EndCause {
    fn from(c: FfiGlassesEndCause) -> Self {
        match c {
            FfiGlassesEndCause::DeviceSignalled => EndCause::DeviceSignalled,
            FfiGlassesEndCause::Interrupted => EndCause::Interrupted,
            FfiGlassesEndCause::IdleTimeout => EndCause::IdleTimeout,
            FfiGlassesEndCause::Disconnected => EndCause::Disconnected,
        }
    }
}

impl From<EndCause> for FfiGlassesEndCause {
    fn from(c: EndCause) -> Self {
        match c {
            EndCause::DeviceSignalled => FfiGlassesEndCause::DeviceSignalled,
            EndCause::Interrupted => FfiGlassesEndCause::Interrupted,
            EndCause::IdleTimeout => FfiGlassesEndCause::IdleTimeout,
            EndCause::Disconnected => FfiGlassesEndCause::Disconnected,
        }
    }
}

/// What one inbound frame did to the voice capture.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesVoiceEvent {
    /// `0x97` — the microphone opened.
    Started,
    /// A `0x46` arrived with no preceding `0x97`: the start was lost and the capture opened
    /// implicitly rather than dropping the utterance.
    StartedRecovered,
    /// One Opus packet. Emitted in the same call as a recovered start, so list order matters.
    Packet { packet: FfiGlassesVoicePacket },
    /// The capture closed.
    Ended { cause: FfiGlassesEndCause },
    /// Not a voice frame, or a close with nothing open.
    Ignored,
}

impl From<VoiceEvent> for FfiGlassesVoiceEvent {
    fn from(e: VoiceEvent) -> Self {
        match e {
            VoiceEvent::Started => FfiGlassesVoiceEvent::Started,
            VoiceEvent::StartedRecovered => FfiGlassesVoiceEvent::StartedRecovered,
            VoiceEvent::Packet(p) => FfiGlassesVoiceEvent::Packet {
                packet: FfiGlassesVoicePacket {
                    payload: p.payload,
                    toc: p.toc.map(Into::into),
                    index: p.index,
                },
            },
            VoiceEvent::Ended(c) => FfiGlassesVoiceEvent::Ended { cause: c.into() },
            VoiceEvent::Ignored => FfiGlassesVoiceEvent::Ignored,
        }
    }
}

/// One decoded inbound frame.
///
/// TOTAL: every possible `cmd` byte produces a variant, so nothing is dropped silently. Frames
/// we recognise but that carry no state land in [`Self::Ack`]; bytes in neither opcode table
/// land in [`Self::Unrecognised`] with their payload intact, which is how an unknown opcode gets
/// characterised without first being enshrined.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesEvent {
    /// `0x17` or `0x53`.
    Battery {
        percent: u8,
        charging: bool,
        source: FfiGlassesBatterySource,
    },
    /// `0x55`.
    Versions { versions: FfiGlassesVersions },
    /// `0x64` — project / customer codes (`"T1"` / `"0303"` on every captured unit).
    Identity { project: String, customer: String },
    /// `0x42` (confirmed) or `0x40` (inferred).
    MediaCount {
        count: u16,
        source: FfiGlassesCountSource,
    },
    /// One field of the `0x48` burst, or a setter echo. Already folded into
    /// [`GlassesParser::switch_states`] by the time the shell sees it.
    Switch { report: FfiGlassesSwitchReport },
    /// `0x07`–`0x11` — a touchpad gesture binding. `action` is `None` when the value byte is
    /// outside the known set; `raw` is always the byte the device sent, so an unmapped binding
    /// is visible rather than reported as "volume down".
    GestureBinding {
        slot: FfiGlassesGestureSlot,
        action: Option<FfiGlassesGestureAction>,
        raw: u8,
    },
    /// `0x95` — the 4-byte capability word, deliberately NOT bit-decoded: four bits are set and
    /// the vendor prints its flag map alphabetically, so the assignment cannot be recovered from
    /// one sample. `known_configuration` says whether this is the hardware we characterised.
    Capabilities { raw: u32, known_configuration: bool },
    /// `0x71` — which voice features are DISABLED. Only ever captured as `0x00`, so the bit
    /// positions are unknown; `all_enabled` is the one reading the evidence supports, and it is
    /// the one that matters — it means the wake word is live.
    VoiceFeatures { raw: u8, all_enabled: bool },
    /// `0x69` — the three output levels, verbatim on an unpinned scale.
    Volumes { system: u8, media: u8, call: u8 },
    /// `0x25` — the SoftAP is up. The passphrase is not on the wire; it is the fixed value
    /// carried here.
    WifiCredentials { ssid: String, passphrase: String },
    /// `0x39` / `0x67` ack. The payload is `01`, NOT the mode byte we sent — code that reads
    /// its own mode back out of this is reading a constant.
    WifiOpening {
        serves: FfiGlassesWifiService,
        raw: u8,
    },
    /// `0x45` — the ten action flags. `active` lists the set ones in index order.
    ActionSync {
        active: Vec<FfiGlassesDeviceAction>,
        /// Bit *i* = payload index *i*. For logging and equality only.
        raw: u16,
    },
    /// Something happened to the wake-word capture.
    Voice { event: FfiGlassesVoiceEvent },
    /// A voice frame reached the STATELESS decode path. A routing instruction, not a decode —
    /// [`GlassesParser`] never emits this, because it owns a voice stream and emits
    /// [`Self::Voice`] instead.
    VoiceFrame { cmd: u8, data: Vec<u8> },
    /// `0x49` — the device abandoned the capture. CLIENT-ONLY, never observed.
    VoiceAbandoned,
    /// `0x51`. CLIENT-ONLY, never observed.
    AiBroadcastCancelled,
    /// `0x52`. CLIENT-ONLY, never observed. Unrelated to the `52 58` file magic.
    HdImageFailed,
    /// `0x96`. CLIENT-ONLY, never observed.
    IspUpgradeFinished { ok: bool },
    /// A recognised opcode carrying no state worth a type — a setter echo, or a probe reply.
    /// `data` is kept because for `0x36` it is the entire result.
    Ack {
        command: FfiGlassesCommand,
        data: Vec<u8>,
    },
    /// The layout did not hold. Carries the raw frame so a shell can log exactly what arrived;
    /// dropping it is how a frozen readout produces no evidence at all.
    Malformed {
        cmd: u8,
        data: Vec<u8>,
        reason: FfiGlassesMalformedReason,
    },
    /// A `cmd` byte in neither opcode table. A real answer, not a failure — the documented way
    /// to characterise an opcode is to probe it and read this.
    Unrecognised { cmd: u8, data: Vec<u8> },
}

impl From<DeviceEvent> for FfiGlassesEvent {
    fn from(e: DeviceEvent) -> Self {
        match e {
            DeviceEvent::Battery(b) => FfiGlassesEvent::Battery {
                percent: b.percent,
                charging: b.charging,
                source: match b.source {
                    parser::BatterySource::ReadReply => FfiGlassesBatterySource::ReadReply,
                    parser::BatterySource::Push => FfiGlassesBatterySource::Push,
                },
            },
            DeviceEvent::Versions(v) => FfiGlassesEvent::Versions {
                versions: FfiGlassesVersions {
                    bt_major: v.bt[0],
                    bt_minor: v.bt[1],
                    bt_patch: v.bt[2],
                    isp_major: v.isp[0],
                    isp_minor: v.isp[1],
                    isp_patch: v.isp[2],
                    hardware: v.hardware,
                },
            },
            DeviceEvent::Identity(i) => FfiGlassesEvent::Identity {
                project: i.project,
                customer: i.customer,
            },
            DeviceEvent::MediaCount(m) => FfiGlassesEvent::MediaCount {
                count: m.count,
                source: match m.source {
                    parser::CountSource::Push => FfiGlassesCountSource::Push,
                    parser::CountSource::ReadReply => FfiGlassesCountSource::ReadReply,
                },
            },
            DeviceEvent::Switch(r) => FfiGlassesEvent::Switch { report: r.into() },
            DeviceEvent::GestureBinding { slot, action, raw } => FfiGlassesEvent::GestureBinding {
                slot: slot.into(),
                action: action.map(Into::into),
                raw,
            },
            DeviceEvent::Capabilities(c) => FfiGlassesEvent::Capabilities {
                raw: c.raw,
                known_configuration: c.is_known_configuration(),
            },
            DeviceEvent::VoiceFeatures(f) => FfiGlassesEvent::VoiceFeatures {
                raw: f.raw,
                all_enabled: f.all_enabled(),
            },
            DeviceEvent::Volumes(v) => FfiGlassesEvent::Volumes {
                system: v.system,
                media: v.media,
                call: v.call,
            },
            DeviceEvent::WifiCredentials(c) => FfiGlassesEvent::WifiCredentials {
                passphrase: c.passphrase().to_string(),
                ssid: c.ssid,
            },
            DeviceEvent::WifiOpening { serves, raw } => FfiGlassesEvent::WifiOpening {
                serves: serves.into(),
                raw,
            },
            DeviceEvent::ActionSync(a) => FfiGlassesEvent::ActionSync {
                active: a.active().into_iter().map(Into::into).collect(),
                raw: a.raw(),
            },
            DeviceEvent::Voice(v) => FfiGlassesEvent::Voice { event: v.into() },
            DeviceEvent::VoiceFrame { cmd, data } => FfiGlassesEvent::VoiceFrame { cmd, data },
            DeviceEvent::VoiceAbandoned => FfiGlassesEvent::VoiceAbandoned,
            DeviceEvent::AiBroadcastCancelled => FfiGlassesEvent::AiBroadcastCancelled,
            DeviceEvent::HdImageFailed => FfiGlassesEvent::HdImageFailed,
            DeviceEvent::IspUpgradeFinished { ok } => FfiGlassesEvent::IspUpgradeFinished { ok },
            DeviceEvent::Ack { command, data } => FfiGlassesEvent::Ack {
                command: command.into(),
                data,
            },
            DeviceEvent::Malformed { cmd, data, reason } => FfiGlassesEvent::Malformed {
                cmd,
                data,
                reason: reason.into(),
            },
            DeviceEvent::Unrecognised { cmd, data } => FfiGlassesEvent::Unrecognised { cmd, data },
        }
    }
}

/// Decode ONE already-deframed frame, statelessly.
///
/// Voice frames come back as [`FfiGlassesEvent::VoiceFrame`] rather than decoded, because the
/// lost-start recovery needs memory. Use [`GlassesParser`], which owns that memory.
#[uniffi::export]
pub fn glasses_parse_frame(cmd: u8, data: Vec<u8>) -> FfiGlassesEvent {
    parser::parse(&Frame::new(cmd, data)).into()
}

// ---------------------------------------------------------------------------------------
// Residual
// ---------------------------------------------------------------------------------------

/// What a deframer could not turn into frames.
///
/// [`Self::Foreign`] is the load-bearing case: on `AA15` those bytes are the `52 58` file
/// stream, and dropping them loses image data. Route them into [`GlassesFileReassembler`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesResidual {
    /// Every byte was consumed.
    Empty,
    /// A prefix that could still become a frame; it stays buffered for the next chunk.
    PartialFrame { buffered: u32 },
    /// Bytes that cannot begin a frame. Handed back, never dropped.
    Foreign { bytes: Vec<u8> },
    /// A partial frame grew past the maximum length without completing and was discarded rather
    /// than allowed to absorb the channel forever.
    Overflowed { dropped: u32 },
}

impl From<Residual> for FfiGlassesResidual {
    fn from(r: Residual) -> Self {
        match r {
            Residual::Empty => FfiGlassesResidual::Empty,
            Residual::PartialFrame(n) => FfiGlassesResidual::PartialFrame { buffered: n as u32 },
            Residual::Foreign(bytes) => FfiGlassesResidual::Foreign { bytes },
            Residual::Overflowed(n) => FfiGlassesResidual::Overflowed { dropped: n as u32 },
        }
    }
}

/// One [`GlassesParser::push_detailed`] result.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesParsed {
    pub events: Vec<FfiGlassesEvent>,
    pub residual: FfiGlassesResidual,
}

/// One [`GlassesDeframer::push_detailed`] result.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesDeframed {
    pub frames: Vec<FfiGlassesFrame>,
    pub residual: FfiGlassesResidual,
}

// ---------------------------------------------------------------------------------------
// Switch states
// ---------------------------------------------------------------------------------------

/// The accumulated `0x48` burst. Every field is optional because "not reported" is a real
/// state, distinct from any value.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesSwitchStates {
    /// `0..=2`.
    pub led: Option<u8>,
    pub record_seconds: Option<u16>,
    pub wear_detection: Option<bool>,
    /// The wake word.
    pub voice_command: Option<bool>,
    pub orientation: Option<FfiGlassesOrientation>,
    /// One entry per slot in [`glasses_gesture_slots`] order — `None` where the slot has not
    /// been reported.
    pub gestures: Vec<Option<FfiGlassesGestureAction>>,
    /// The value byte exactly as reported, even where it maps to no known action.
    pub gestures_raw: Vec<Option<u8>>,
    /// True once every field the burst reports has arrived — **including all five gestures**.
    /// The reference iOS implementation's own accumulator reports complete without them, so "settings synced" there
    /// means the touch configuration is still entirely unknown.
    pub is_complete: bool,
}

impl From<&SwitchStates> for FfiGlassesSwitchStates {
    fn from(s: &SwitchStates) -> Self {
        Self {
            led: s.led(),
            record_seconds: s.record_seconds(),
            wear_detection: s.wear_detection(),
            voice_command: s.voice_command(),
            orientation: s.orientation().map(Into::into),
            gestures: GestureSlot::ALL
                .into_iter()
                .map(|slot| s.gesture(slot).map(Into::into))
                .collect(),
            gestures_raw: GestureSlot::ALL
                .into_iter()
                .map(|slot| s.gesture_raw(slot))
                .collect(),
            is_complete: s.is_complete(),
        }
    }
}

/// The switch-state accumulator on its own, for a shell that routes events itself.
///
/// [`GlassesParser`] already owns one and folds every event into it, so most callers never need
/// this. It is exported for the shell that drives the burst through its own dispatcher, and so
/// a settings screen can accumulate a replay without standing up a parser.
#[derive(uniffi::Object)]
pub struct GlassesSwitchStates {
    inner: Mutex<SwitchStates>,
}

#[uniffi::export]
impl GlassesSwitchStates {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SwitchStates::new()),
        })
    }

    /// Fold one raw frame in. Returns true when it WAS a settings frame, so a caller routing a
    /// stream knows whether it still has to handle it.
    ///
    /// Takes the frame rather than an event because an event cannot cross back in as an
    /// argument without the shell re-encoding what this crate just decoded.
    pub fn ingest_frame(&self, cmd: u8, data: Vec<u8>) -> bool {
        let event = parser::parse(&Frame::new(cmd, data));
        self.lock().ingest(&event)
    }

    pub fn snapshot(&self) -> FfiGlassesSwitchStates {
        FfiGlassesSwitchStates::from(&*self.lock())
    }

    /// True once all ten burst fields have arrived, gestures included.
    pub fn is_complete(&self) -> bool {
        self.lock().is_complete()
    }

    /// Forget everything reported. The shell calls this on disconnect, so a new session cannot
    /// inherit the previous device's settings.
    pub fn reset(&self) {
        self.lock().reset();
    }
}

impl GlassesSwitchStates {
    fn lock(&self) -> std::sync::MutexGuard<'_, SwitchStates> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------------------

/// The control-channel pump: bytes in, decoded events out.
///
/// Owns a deframer, the voice state machine and a [`SwitchStates`] accumulator. `Mutex` rather
/// than `RefCell` because UniFFI objects are handed out as `Arc` with `&self` methods; it is
/// uncontended in practice (one BLE delegate queue drives it) and every method is short and
/// non-blocking.
///
/// **One instance per characteristic.** `AA14` and `AA15` are independent byte streams, and
/// sharing a deframer between them interleaves two half-frames into one corrupt one.
#[derive(uniffi::Object)]
pub struct GlassesParser {
    inner: Mutex<ParserState>,
}

struct ParserState {
    parser: Parser,
    switches: SwitchStates,
}

impl ParserState {
    fn fold(&mut self, events: Vec<DeviceEvent>) -> Vec<FfiGlassesEvent> {
        events
            .into_iter()
            .map(|e| {
                self.switches.ingest(&e);
                FfiGlassesEvent::from(e)
            })
            .collect()
    }
}

#[uniffi::export]
impl GlassesParser {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ParserState {
                parser: Parser::new(),
                switches: SwitchStates::new(),
            }),
        })
    }

    /// Feed one notification's bytes.
    ///
    /// Use [`Self::push_detailed`] on the file characteristic, where the residual is the `52 58`
    /// stream and dropping it loses image data.
    pub fn push(&self, chunk: Vec<u8>) -> Vec<FfiGlassesEvent> {
        let mut g = self.lock();
        let events = g.parser.push(&chunk);
        g.fold(events)
    }

    /// [`Self::push`], plus what happened to the bytes that did not become frames.
    pub fn push_detailed(&self, chunk: Vec<u8>) -> FfiGlassesParsed {
        let mut g = self.lock();
        let (events, residual) = g.parser.push_detailed(&chunk);
        FfiGlassesParsed {
            events: g.fold(events),
            residual: residual.into(),
        }
    }

    /// Decode one already-deframed frame, applying the voice state machine. For a shell that
    /// does its own framing, or a test replaying captured frames.
    pub fn feed_frame(&self, cmd: u8, data: Vec<u8>) -> Vec<FfiGlassesEvent> {
        let mut g = self.lock();
        let events = g.parser.feed(&Frame::new(cmd, data));
        g.fold(events)
    }

    /// True while a voice capture is open.
    pub fn is_capturing(&self) -> bool {
        self.lock().parser.is_capturing()
    }

    /// End the capture for a reason only the shell knows.
    ///
    /// **This is the method that matters most on this firmware.** `0x56` is an app → device
    /// WRITE that never appears on the stream being parsed, and it is the only thing that closes
    /// the mic: four captures totalling 14,636 voice frames contain no `0x99` and no gap. A
    /// parser that waits to OBSERVE the close waits forever. The idle DURATION behind
    /// [`FfiGlassesEndCause::IdleTimeout`] is the shell's alone.
    pub fn close_voice(&self, cause: FfiGlassesEndCause) -> Vec<FfiGlassesEvent> {
        let mut g = self.lock();
        let events = g.parser.close_voice(cause.into());
        g.fold(events)
    }

    /// Bytes held that have not yet completed a frame — diagnostics only.
    pub fn buffered(&self) -> u32 {
        self.lock().parser.buffered().len() as u32
    }

    /// The accumulated `0x48` burst, folded from every event this parser has emitted.
    pub fn switch_states(&self) -> FfiGlassesSwitchStates {
        FfiGlassesSwitchStates::from(&self.lock().switches)
    }

    /// Forget the frame buffer, the capture state and the accumulated settings.
    ///
    /// The shell calls this on disconnect, where a half-frame from the previous link would
    /// otherwise prefix the next one and a stale "capturing" would swallow the next session's
    /// recovered start. Prefer [`Self::close_voice`] with
    /// [`FfiGlassesEndCause::Disconnected`] when a capture was open — it says what happened
    /// instead of discarding it silently.
    pub fn reset(&self) {
        let mut g = self.lock();
        g.parser.reset();
        g.switches.reset();
    }
}

impl GlassesParser {
    fn lock(&self) -> std::sync::MutexGuard<'_, ParserState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The raw frame demuxer, without the decode.
///
/// [`GlassesParser`] is what a shell driving glasses wants. This is for capture analysis and
/// for reading back our own outbound stream ([`Self::app`]), where the frames matter and the
/// events do not.
#[derive(uniffi::Object)]
pub struct GlassesDeframer {
    inner: Mutex<Deframer>,
}

#[uniffi::export]
impl GlassesDeframer {
    /// For the device → app channels (`AA14`, `AA15`).
    #[uniffi::constructor]
    pub fn device() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Deframer::device()),
        })
    }

    /// For reading back our own outbound (`AB 55`) stream.
    #[uniffi::constructor]
    pub fn app() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Deframer::app()),
        })
    }

    pub fn push(&self, chunk: Vec<u8>) -> Vec<FfiGlassesFrame> {
        self.lock()
            .push(&chunk)
            .iter()
            .map(FfiGlassesFrame::from)
            .collect()
    }

    pub fn push_detailed(&self, chunk: Vec<u8>) -> FfiGlassesDeframed {
        let (frames, residual) = self.lock().push_detailed(&chunk);
        FfiGlassesDeframed {
            frames: frames.iter().map(FfiGlassesFrame::from).collect(),
            residual: residual.into(),
        }
    }

    pub fn buffered(&self) -> u32 {
        self.lock().buffered().len() as u32
    }

    pub fn reset(&self) {
        self.lock().reset();
    }
}

impl GlassesDeframer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Deframer> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------------------

/// The `0x97` → `0x46`… → close state machine on its own.
///
/// [`GlassesParser`] owns one and drives it; this is for a shell that routes frames itself.
/// **Control-channel frames only** — a `52 58` file frame carries `0x97` and `0x99` with
/// entirely different meanings, and feeding one here opens and closes a microphone capture
/// every time a photo transfers.
#[derive(uniffi::Object)]
pub struct GlassesVoiceStream {
    inner: Mutex<VoiceStream>,
}

#[uniffi::export]
impl GlassesVoiceStream {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VoiceStream::new()),
        })
    }

    /// Drive the machine with one decoded `AC 55` frame. Returns 0, 1 or 2 events — two when a
    /// lost `0x97` is recovered.
    pub fn handle(&self, cmd: u8, data: Vec<u8>) -> Vec<FfiGlassesVoiceEvent> {
        self.lock()
            .handle_upload(cmd, &data)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Close the capture for a reason the shell knows and this cannot: the `0x56` write went
    /// out, an idle timer expired, or the link dropped.
    pub fn close(&self, cause: FfiGlassesEndCause) -> Vec<FfiGlassesVoiceEvent> {
        self.lock()
            .close(cause.into())
            .into_iter()
            .map(Into::into)
            .collect()
    }

    pub fn is_capturing(&self) -> bool {
        self.lock().is_capturing()
    }

    /// How many `0x46` packets the current (or most recent) capture carried.
    pub fn packets_in_capture(&self) -> u64 {
        self.lock().packets_in_capture()
    }

    /// Forget the capture without emitting anything. Prefer [`Self::close`], which says what
    /// happened.
    pub fn reset(&self) {
        self.lock().reset();
    }
}

impl GlassesVoiceStream {
    fn lock(&self) -> std::sync::MutexGuard<'_, VoiceStream> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The sample rate to request from the native Opus decoder for the captured stream.
#[uniffi::export]
pub fn glasses_voice_sample_rate_hz() -> u32 {
    crate::voice::REQUESTED_SAMPLE_RATE_HZ
}

/// Bits per sample of the PCM the native decoder returns.
#[uniffi::export]
pub fn glasses_voice_bits_per_sample() -> u32 {
    crate::voice::BITS_PER_SAMPLE
}

/// Channel count of the captured stream.
#[uniffi::export]
pub fn glasses_voice_channels() -> u32 {
    crate::voice::CHANNELS
}

/// Parse one Opus packet's header, without a capture in flight.
#[uniffi::export]
pub fn glasses_parse_opus_toc(packet: Vec<u8>) -> Option<FfiGlassesOpusToc> {
    OpusToc::parse(&packet).map(Into::into)
}

// ---------------------------------------------------------------------------------------
// File reassembly
// ---------------------------------------------------------------------------------------

/// The `0x97` header's type byte. Only [`Self::HdImage`] has ever been seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesFileKind {
    /// CLIENT-ONLY.
    ImageThumb,
    /// The type byte in all three captured transfers.
    HdImage,
    /// CLIENT-ONLY.
    VideoThumb,
}

impl From<FileKind> for FfiGlassesFileKind {
    fn from(k: FileKind) -> Self {
        match k {
            FileKind::ImageThumb => FfiGlassesFileKind::ImageThumb,
            FileKind::HdImage => FfiGlassesFileKind::HdImage,
            FileKind::VideoThumb => FfiGlassesFileKind::VideoThumb,
        }
    }
}

/// Why a transfer ended without producing a file.
///
/// Both reference implementations return an empty list in every one of these cases, which is why a photo that was
/// most of the way across and a device that never answered were indistinguishable from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesFileAbort {
    /// A `0x99` arrived with no transfer open — the `0x97` header was lost.
    NoFileInfo,
    /// A `0x97` declared zero bytes, or more than the cap. Nothing was allocated on it.
    ImplausibleSize { declared_total: u32 },
    /// A `0x97` arrived while a transfer was still in flight. Both reference implementations restart silently and
    /// the partial file evaporates.
    Superseded { received: u32, declared_total: u32 },
    /// A `0x98` addressed bytes outside the declared file.
    OutOfRange {
        addr: u32,
        end: u32,
        declared_total: u32,
    },
    /// A `0x99` arrived with fewer bytes GENUINELY delivered than declared. `first_gap` is the
    /// offset of the first byte never received.
    Incomplete {
        received: u32,
        declared_total: u32,
        first_gap: Option<u32>,
    },
}

impl From<FileAbort> for FfiGlassesFileAbort {
    fn from(a: FileAbort) -> Self {
        match a {
            FileAbort::NoFileInfo => FfiGlassesFileAbort::NoFileInfo,
            FileAbort::ImplausibleSize { declared_total } => FfiGlassesFileAbort::ImplausibleSize {
                declared_total: declared_total as u32,
            },
            FileAbort::Superseded {
                received,
                declared_total,
            } => FfiGlassesFileAbort::Superseded {
                received: received as u32,
                declared_total: declared_total as u32,
            },
            FileAbort::OutOfRange {
                addr,
                end,
                declared_total,
            } => FfiGlassesFileAbort::OutOfRange {
                addr: addr as u32,
                end: end as u32,
                declared_total: declared_total as u32,
            },
            FileAbort::Incomplete {
                received,
                declared_total,
                first_gap,
            } => FfiGlassesFileAbort::Incomplete {
                received: received as u32,
                declared_total: declared_total as u32,
                first_gap: first_gap.map(|g| g as u32),
            },
        }
    }
}

/// What a chunk of file-channel bytes did to the transfer state.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGlassesFileEvent {
    /// A `0x97` header opened a transfer.
    Started { declared_total: u32, file_type: u8 },
    /// A `0x99` closed one and every declared byte was genuinely delivered.
    Completed {
        /// The raw type byte, kept raw because the wire is the authority on it.
        file_type: u8,
        /// The named value, or `None` for a byte no client declares.
        kind: Option<FfiGlassesFileKind>,
        data: Vec<u8>,
    },
    /// The transfer ended with nothing to hand over.
    Aborted { abort: FfiGlassesFileAbort },
    /// Bytes that could not begin a frame were discarded to regain sync. Surfaced rather than
    /// swallowed: on a channel carrying only this framing, a desync is a symptom.
    Desynced { dropped: u32 },
}

impl From<FileEvent> for FfiGlassesFileEvent {
    fn from(e: FileEvent) -> Self {
        match e {
            FileEvent::Started {
                declared_total,
                file_type,
            } => FfiGlassesFileEvent::Started {
                declared_total: declared_total as u32,
                file_type,
            },
            FileEvent::Completed(f) => FfiGlassesFileEvent::Completed {
                file_type: f.file_type,
                kind: f.kind().map(Into::into),
                data: f.data,
            },
            FileEvent::Aborted(a) => FfiGlassesFileEvent::Aborted { abort: a.into() },
            FileEvent::Desynced { dropped } => FfiGlassesFileEvent::Desynced {
                dropped: dropped as u32,
            },
        }
    }
}

/// How far along the in-flight transfer is.
///
/// `received` counts bytes a `0x98` genuinely delivered and never a gap's zero-fill, so it
/// cannot be inflated by padding — which is the whole difference between the two reference implementations'
/// completeness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FfiGlassesTransferProgress {
    pub received: u32,
    pub declared_total: u32,
    pub file_type: u8,
    /// Offset of the first byte not yet delivered, or `None` when the file is complete.
    pub first_gap: Option<u32>,
    /// Whether a `0x97` has opened a transfer that has not yet closed.
    pub is_active: bool,
}

/// Streaming accumulator for the `52 58` file channel.
///
/// Feed it the [`FfiGlassesResidual::Foreign`] bytes a [`GlassesParser`] on the file
/// characteristic hands back; take completed files and failures out.
#[derive(uniffi::Object)]
pub struct GlassesFileReassembler {
    inner: Mutex<FileReassembler>,
}

#[uniffi::export]
impl GlassesFileReassembler {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FileReassembler::new()),
        })
    }

    /// Feed one chunk of file-channel bytes; returns what happened.
    pub fn push(&self, chunk: Vec<u8>) -> Vec<FfiGlassesFileEvent> {
        self.lock()
            .push(&chunk)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Route one already-decoded file frame, for a shell that does its own framing.
    pub fn handle_frame(&self, cmd: u8, data: Vec<u8>) -> Vec<FfiGlassesFileEvent> {
        self.lock()
            .handle_frame(&Frame::new(cmd, data))
            .into_iter()
            .map(Into::into)
            .collect()
    }

    pub fn progress(&self) -> FfiGlassesTransferProgress {
        let p = self.lock().progress();
        FfiGlassesTransferProgress {
            received: p.received as u32,
            declared_total: p.declared_total as u32,
            file_type: p.file_type,
            first_gap: p.first_gap.map(|g| g as u32),
            is_active: p.is_active(),
        }
    }

    /// Whether a partial frame is buffered. The iOS demux uses this to decide whether an
    /// `AC 55`-looking chunk is a genuine control frame or JPEG bytes that happen to start with
    /// the magic.
    pub fn has_buffered_frame_data(&self) -> bool {
        self.lock().has_buffered_frame_data()
    }

    pub fn buffered(&self) -> u32 {
        self.lock().buffered().len() as u32
    }

    /// Forget everything: buffered bytes and any transfer in flight. The shell calls this on
    /// disconnect and on its own transfer timeout — this core has no clock and cannot notice
    /// that the device went quiet.
    pub fn reset(&self) {
        self.lock().reset();
    }
}

impl GlassesFileReassembler {
    fn lock(&self) -> std::sync::MutexGuard<'_, FileReassembler> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Build one `52 58` file frame. For tests, fixtures and replay tooling — this crate never plays
/// device on a radio.
#[uniffi::export]
pub fn glasses_encode_file_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
    crate::reassembly::encode_file_frame(cmd, &payload)
}

/// The largest file this reassembler will accept a header for.
#[uniffi::export]
pub fn glasses_max_file_bytes() -> u32 {
    crate::reassembly::MAX_FILE_BYTES as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::encode_device;

    /// The whole inbound path in one pass, on the bytes a real device sends.
    #[test]
    fn a_control_stream_crosses_the_boundary_as_typed_events() {
        let p = GlassesParser::new();
        // `ac 55 00 04 17 34 39 01` — 49 %, charging. Captured.
        let events = p.push(encode_device(0x17, &[0x34, 0x39, 0x01]));
        assert_eq!(
            events,
            vec![FfiGlassesEvent::Battery {
                percent: 49,
                charging: true,
                source: FfiGlassesBatterySource::ReadReply,
            }]
        );
    }

    /// The ten-frame burst, in capture order, accumulated by the parser itself — the shell
    /// never sees a partial settings object it has to assemble.
    #[test]
    fn the_switch_state_burst_accumulates_inside_the_parser() {
        let p = GlassesParser::new();
        let burst: [(u8, Vec<u8>); 10] = [
            (0x01, vec![b'1']),
            (0x02, vec![0x00, 0x3C]),
            (0x04, vec![b'1']),
            (0x06, vec![b'1']),
            (0x07, vec![b'0']),
            (0x08, vec![b'1']),
            (0x09, vec![b'2']),
            (0x10, vec![b'4']),
            (0x11, vec![b'3']),
            (0x61, vec![b'0']),
        ];
        for (cmd, data) in &burst {
            assert!(!p.switch_states().is_complete, "not complete until the last frame");
            p.push(encode_device(*cmd, data));
        }
        let s = p.switch_states();
        assert!(s.is_complete, "all ten fields, gestures included");
        assert_eq!(s.led, Some(1));
        assert_eq!(s.record_seconds, Some(60));
        assert_eq!(s.wear_detection, Some(true));
        assert_eq!(s.voice_command, Some(true));
        assert_eq!(s.orientation, Some(FfiGlassesOrientation::Portrait));
        assert_eq!(
            s.gestures,
            vec![
                Some(FfiGlassesGestureAction::VolumeDown),
                Some(FfiGlassesGestureAction::VolumeUp),
                Some(FfiGlassesGestureAction::PlayPause),
                Some(FfiGlassesGestureAction::PreviousTrack),
                Some(FfiGlassesGestureAction::NextTrack),
            ]
        );
    }

    /// There is no `0x48` reply, and the exported burst order is what a shell must wait on
    /// instead. A shell that waits for a `0x48` frame waits forever.
    #[test]
    fn the_burst_order_is_exported_and_contains_no_zero_x48() {
        let order = glasses_switch_state_burst_order();
        assert_eq!(order.len(), 10);
        assert!(!order.contains(&FfiGlassesCommand::GetSwitchStates));
        assert_eq!(
            order.iter().filter(|c| glasses_command_code(**c) >= 0x07 && glasses_command_code(**c) <= 0x11).count(),
            5,
            "the five gesture slots are in the burst"
        );
    }

    /// The `AA15` demux: control frames come back as events and the `52 58` bytes come back as
    /// `Foreign` for the file reassembler. Dropping that residual loses image data.
    #[test]
    fn the_file_channel_hands_foreign_bytes_back_for_the_reassembler() {
        let p = GlassesParser::new();
        let files = GlassesFileReassembler::new();

        let mut chunk = encode_device(0x53, &[0x01, 0x40]); // battery push, 64 %
        chunk.extend(glasses_encode_file_frame(0x97, vec![0, 0, 0, 4, 0x02])); // 4-byte HD image
        let parsed = p.push_detailed(chunk);
        assert_eq!(
            parsed.events,
            vec![FfiGlassesEvent::Battery {
                percent: 0x40,
                charging: true,
                source: FfiGlassesBatterySource::Push,
            }]
        );
        let FfiGlassesResidual::Foreign { bytes } = parsed.residual else {
            panic!("the 52 58 stream must come back, not be dropped: {:?}", parsed.residual);
        };
        assert_eq!(
            files.push(bytes),
            vec![FfiGlassesFileEvent::Started {
                declared_total: 4,
                file_type: 0x02
            }]
        );

        files.push(glasses_encode_file_frame(0x98, vec![0, 0, 0, 0, 1, 2, 3, 4]));
        assert_eq!(
            files.push(glasses_encode_file_frame(0x99, vec![0x00])),
            vec![FfiGlassesFileEvent::Completed {
                file_type: 0x02,
                kind: Some(FfiGlassesFileKind::HdImage),
                data: vec![1, 2, 3, 4],
            }]
        );
    }

    /// The mic close has to be TOLD to the parser — no capture contains a `0x99`, so a shell
    /// waiting to observe one waits forever.
    #[test]
    fn a_voice_capture_opens_on_the_wire_and_closes_only_when_told() {
        let p = GlassesParser::new();
        assert!(!p.is_capturing());
        assert_eq!(p.push(encode_device(0x97, &[0x00])).len(), 1);
        assert!(p.is_capturing());

        let events = p.push(encode_device(0x46, &[0x48, 0x01, 0x02]));
        let FfiGlassesEvent::Voice {
            event: FfiGlassesVoiceEvent::Packet { packet },
        } = &events[0]
        else {
            panic!("expected a packet, got {events:?}");
        };
        assert_eq!(packet.index, 0);
        assert_eq!(packet.payload, vec![0x48, 0x01, 0x02]);

        let closed = p.close_voice(FfiGlassesEndCause::Interrupted);
        assert_eq!(
            closed,
            vec![FfiGlassesEvent::Voice {
                event: FfiGlassesVoiceEvent::Ended {
                    cause: FfiGlassesEndCause::Interrupted
                }
            }]
        );
        assert!(!p.is_capturing());
    }

    /// A `0x46` with no preceding `0x97` recovers the lost start rather than dropping the
    /// utterance, and the recovery arrives BEFORE the packet in the same call.
    #[test]
    fn a_lost_voice_start_is_recovered_in_order() {
        let p = GlassesParser::new();
        let events = p.push(encode_device(0x46, &[0x48, 0x01]));
        assert!(matches!(
            events.as_slice(),
            [
                FfiGlassesEvent::Voice {
                    event: FfiGlassesVoiceEvent::StartedRecovered
                },
                FfiGlassesEvent::Voice {
                    event: FfiGlassesVoiceEvent::Packet { .. }
                }
            ]
        ));
    }

    /// Index 9 is file-IMPORT mode, not "Wi-Fi active". The reference iOS implementation publishes it as
    /// `wifiActive`; the name that crosses here is the vendor's own.
    #[test]
    fn action_sync_index_nine_is_importing_mode() {
        assert_eq!(
            glasses_device_actions().last(),
            Some(&FfiGlassesDeviceAction::ImportingMode)
        );
        let p = GlassesParser::new();
        let mut flags = [0u8; 10];
        flags[9] = 1;
        let events = p.push(encode_device(0x45, &flags));
        assert_eq!(
            events,
            vec![FfiGlassesEvent::ActionSync {
                active: vec![FfiGlassesDeviceAction::ImportingMode],
                raw: 1 << 9,
            }]
        );
    }

    /// The three flags no device has ever set stay marked as such, so a UI binding on one is a
    /// visible choice rather than an accident.
    #[test]
    fn the_never_observed_action_flags_are_marked_client_only() {
        for a in [
            FfiGlassesDeviceAction::NodHead,
            FfiGlassesDeviceAction::ShakeHead,
            FfiGlassesDeviceAction::WearingDetection,
        ] {
            assert_eq!(glasses_device_action_evidence(a), FfiGlassesEvidence::ClientOnly);
        }
        assert_eq!(
            glasses_device_action_evidence(FfiGlassesDeviceAction::ImportingMode),
            FfiGlassesEvidence::Capture
        );
    }

    /// Every opcode round-trips through the boundary enum by BYTE, which is what stops the two
    /// tables drifting apart the way the reference implementations' did.
    #[test]
    fn every_opcode_round_trips_by_wire_byte() {
        let all = glasses_all_commands();
        assert_eq!(all.len(), AppCommand::ALL.len());
        for cmd in all {
            let code = glasses_command_code(cmd);
            assert_eq!(glasses_command_from_code(code), Some(cmd));
        }
        assert_eq!(glasses_command_from_code(0xEE), None, "an unknown byte is a real answer");
    }

    #[test]
    fn the_encoders_round_trip_through_the_decoders() {
        let frame = glasses_encode_command(FfiGlassesCommand::GetBattery, Vec::new());
        assert_eq!(frame, vec![0xAB, 0x55, 0x00, 0x03, 0x17, 0x00, 0x17]);
        assert_eq!(
            glasses_decode_app(frame),
            FfiGlassesDecoded::Ok {
                frame: FfiGlassesFrame {
                    cmd: 0x17,
                    data: vec![0x00]
                }
            }
        );
        // A corrupt checksum is reported, not thrown, and stays distinguishable from a
        // truncation — the deframer waits on one and resyncs on the other.
        let mut bad = encode_device(0x17, &[0x34, 0x39, 0x01]);
        *bad.last_mut().unwrap() ^= 0xFF;
        assert_eq!(
            glasses_decode_device(bad),
            FfiGlassesDecoded::Err {
                reason: FfiGlassesDecodeError::ChecksumMismatch
            }
        );
    }

    /// The GATT answer both reference implementations need, including the one Android has wrong.
    #[test]
    fn the_gatt_table_states_the_write_mode() {
        let g = glasses_gatt();
        assert_eq!(g.service, "AA12");
        assert_eq!(g.write, "AA13");
        assert_eq!(g.notify_all, vec!["AA14", "AA15"]);
        assert!(g.write_with_response, "every captured write is an ATT Write Request");
        assert!(glasses_is_drivable(vec!["AA13".into(), "AA14".into()]));
        assert!(!glasses_is_drivable(vec!["AA14".into(), "AA15".into()]));
    }

    /// The builders and the parser agree on the wire in both directions: what we send for a
    /// setting is what the device echoes back for it.
    #[test]
    fn a_setting_write_and_its_echo_are_the_same_bytes() {
        let sent = glasses_set_voice_command(true);
        let FfiGlassesDecoded::Ok { frame } = glasses_decode_app(sent) else {
            panic!("our own frame must decode");
        };
        let p = GlassesParser::new();
        p.push(encode_device(frame.cmd, &frame.data));
        assert_eq!(p.switch_states().voice_command, Some(true));
    }

    /// A reported setting must map back to the enum the SETTER takes, through the boundary and
    /// not through arithmetic each shell writes for itself. This is the round trip iOS actually
    /// performs — read the level, store it, push it back — so an off-by-`0x30` here sends the
    /// hardware a brightness it never reported.
    #[test]
    fn a_reported_led_level_maps_back_to_the_setter_enum() {
        for level in glasses_led_levels() {
            let reported = glasses_led_level_value(level);
            assert_eq!(glasses_led_level_from_value(reported), Some(level));

            // …and the reported value is what the PARSER produces for our own setter's bytes.
            let FfiGlassesDecoded::Ok { frame } = glasses_decode_app(glasses_set_led(level)) else {
                panic!("our own frame must decode");
            };
            let p = GlassesParser::new();
            p.push(encode_device(frame.cmd, &frame.data));
            assert_eq!(p.switch_states().led, Some(reported));
        }
        assert_eq!(glasses_led_level_from_value(3), None, "no silent clamp");
    }

    /// The same round trip for a gesture binding: the raw byte the burst reports is the byte
    /// our setter writes, and both sides agree through the exported inverse pair.
    #[test]
    fn a_reported_gesture_binding_maps_back_to_the_setter_enum() {
        let actions = glasses_gesture_actions();
        assert_eq!(actions.len(), 5);
        for action in actions {
            let wire = glasses_gesture_action_value(action);
            assert_eq!(glasses_gesture_action_from_value(wire), Some(action));

            for slot in glasses_gesture_slots() {
                let FfiGlassesDecoded::Ok { frame } =
                    glasses_decode_app(glasses_set_gesture(slot, action))
                else {
                    panic!("our own frame must decode");
                };
                let p = GlassesParser::new();
                p.push(encode_device(frame.cmd, &frame.data));
                let s = p.switch_states();
                let i = glasses_gesture_slots().iter().position(|x| *x == slot).unwrap();
                assert_eq!(s.gestures[i], Some(action));
                assert_eq!(s.gestures_raw[i], Some(wire));
            }
        }
    }

    /// The `0x69` reply lists the channels in one order; the exported list IS that order, so a
    /// shell never re-derives it.
    #[test]
    fn the_volume_channel_list_is_the_reply_order() {
        let channels = glasses_volume_channels();
        assert_eq!(
            channels,
            vec![
                FfiGlassesVolumeChannel::System,
                FfiGlassesVolumeChannel::Media,
                FfiGlassesVolumeChannel::Call
            ]
        );
        assert_eq!(
            channels.iter().map(|c| glasses_volume_channel_code(*c)).collect::<Vec<_>>(),
            vec![0x00, 0x01, 0x02]
        );

        let p = GlassesParser::new();
        let events = p.push(encode_device(0x69, &[0x06, 0x07, 0x06]));
        assert_eq!(
            events,
            vec![FfiGlassesEvent::Volumes {
                system: 0x06,
                media: 0x07,
                call: 0x06
            }]
        );
        // …and that is the order `0x70` addresses them in. `ab55000470010778` is the captured
        // "set media volume to 7" write, byte for byte (frame.rs golden vector).
        assert_eq!(
            glasses_set_volume(FfiGlassesVolumeChannel::Media, 0x07),
            vec![0xAB, 0x55, 0x00, 0x04, 0x70, 0x01, 0x07, 0x78]
        );
    }

    #[test]
    fn the_standalone_accumulator_matches_the_parsers() {
        let s = GlassesSwitchStates::new();
        assert!(s.ingest_frame(0x01, vec![b'2']), "a settings frame is consumed");
        assert!(!s.ingest_frame(0x53, vec![0x01, 0x40]), "a battery push is not");
        assert_eq!(s.snapshot().led, Some(2));
        assert!(!s.is_complete());
        s.reset();
        assert_eq!(s.snapshot().led, None);
    }
}
