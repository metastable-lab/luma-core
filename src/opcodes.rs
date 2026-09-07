//! The smart-glasses opcode vocabulary — the UNION of both shipping clients.
//!
//! Port of the reference iOS client's packet codec (`GlassesCmd`,
//! `GlassesSetting`, `GlassesUpload`, `GlassesGestureAction`) cross-checked against
//! its Android counterpart.
//!
//! ## Why this file is the reason the port exists
//!
//! The two clients were counted before anything was written here:
//!
//! | | distinct opcode bytes | app→device names | device→app names |
//! |---|---|---|---|
//! | iOS   | **51** | 41 (36 `GlassesCmd` + 5 gesture slots) | 12 |
//! | Android | **39** | 29 | 11 |
//!
//! Android's set is a strict SUBSET — there is **not one** Android-only opcode. It is not
//! diverged, it is behind, by exactly twelve bytes: the five gesture slots
//! [`AppCommand::SetGestureSwipeForward`]…[`AppCommand::SetGestureTripleTap`] (the entire
//! touch-input surface), plus [`AppCommand::PullImage`], [`AppCommand::PullThumbnailStatus`],
//! [`AppCommand::OpenWifiLive`], [`AppCommand::GetVolumes`], [`AppCommand::SetVolume`],
//! [`AppCommand::GetVoiceDisableState`] and [`AppCommand::GetCapabilities`]. One table closes
//! that gap by construction: a thirteenth cannot open without editing this enum.
//!
//! ## Evidence discipline
//!
//! Every variant carries an [`Evidence`] level, and the level is a fact about *bytes we hold*,
//! not about how confident the Swift comment sounded. This project has shipped a decode derived
//! from a branch no device ever exercised; [`Evidence::ClientOnly`] is the honest label for that
//! situation, and of the 53 names declared here **39 are CAPTURE, 2 are DEVICE-PROBE and 12 are
//! CLIENT-ONLY**. `evidence()` is total and machine-checked, so a new opcode cannot be added
//! without stating what backs it, and `the_evidence_census_matches_the_documented_counts` stops
//! that sentence going stale.
//!
//! ## Direction is not a property of the byte
//!
//! `0x45` and `0x69` are each BOTH an app→device request and a device→app frame, and most
//! setters are echoed back verbatim as an ack — so the inbound stream carries [`AppCommand`]
//! bytes as often as [`DeviceUpload`] ones. The frame codec therefore never classifies: it
//! returns the raw `cmd` byte and lets the router decide, exactly as both reference implementations do.

/// How strongly an opcode is evidenced. Ordered weakest-last on purpose so a `match` reads
/// as a confidence ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Evidence {
    /// Observed on the wire in the packet captures, in at least one direction. The strongest level
    /// available without new hardware.
    Capture,
    /// Not in any capture we hold, but exercised against real hardware and written up in
    /// `PROTOCOL.md` §13b — the firmware's response (or documented silence)
    /// was observed with inbound logging armed and a bogus-opcode control alongside.
    DeviceProbe,
    /// Present only in a client implementation and/or the v07 protocol PDF. Nobody has seen
    /// the device send or accept it. Encoding one is a guess; decoding one is a branch that
    /// may never fire.
    ClientOnly,
}

/// App → device command bytes (protocol §2.1.2), the union of both clients.
///
/// The discriminant IS the wire byte. `#[non_exhaustive]` is deliberately absent: the value of
/// this file is that the set is closed and countable, and [`AppCommand::ALL`] is asserted
/// against a pinned list so a silent addition fails a test rather than a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AppCommand {
    /// `0x01` — indicator-LED brightness, `0x30`/`0x31`/`0x32` (low/mid/high). There is no
    /// "off" value.
    ///
    /// CAPTURE-CONFIRMED as a wire frame in both directions (capture `client_1` writes all
    /// three levels and the device echoes each), and it appears in every `0x48` burst. It is
    /// nevertheless a **dead feature**: the vendor Android APK implements `setLedBrightness`
    /// with zero callers, has no LED UI, no LED constants, no LED capability bit, and does not
    /// even parse the frame on receive (`PROTOCOL.md` §3a). The reference client exposes no
    /// LED control. Present here because the byte is real and arrives in the burst, not
    /// because anything should send it.
    SetLed = 0x01,
    /// `0x02` — video/loop recording duration, seconds, 2-byte BIG-endian.
    ///
    /// The one setting in the `0x48` burst whose value is NOT an ASCII digit. Captured as
    /// `00 3c` (60 s), `00 5a` (90 s) and `00 b4` (180 s).
    SetRecordDuration = 0x02,
    /// `0x04` — wear detection on/off (`0x31`/`0x30`).
    ///
    /// Confirmed only as an INBOUND burst frame (`ac 55 00 03 04 31`); no capture writes it.
    WearDetection = 0x04,
    /// `0x06` — voice-command (wake word) on/off (`0x31`/`0x30`).
    ///
    /// Confirmed only as an inbound burst frame, as with `0x04`.
    VoiceCommand = 0x06,
    /// `0x07` — touchpad **swipe forward** binding. Value is an ASCII digit
    /// ([`GestureAction`]); the factory default is `'0'` = volume down.
    ///
    /// Missing from Android entirely. CAPTURE-CONFIRMED as an inbound frame in
    /// `eyevue_1`, `glassx_1`, `client_1`, `client_2`, `client_3` — always `ac 55 00 03 07 30`.
    /// The slot→GESTURE mapping is INFERRED, see [`GestureSlot`].
    SetGestureSwipeForward = 0x07,
    /// `0x08` — touchpad **swipe back** binding. Captured value `'1'` (volume up) on every
    /// unit. Missing from Android. Slot→gesture mapping INFERRED.
    SetGestureSwipeBack = 0x08,
    /// `0x09` — **single tap** binding. Captured value `'2'` (play/pause) on every unit.
    /// Missing from Android. Slot→gesture mapping INFERRED.
    SetGestureSingleTap = 0x09,
    /// `0x10` — **double tap** binding. Captured value `'4'` (previous track).
    ///
    /// Note `0x10`/`0x11` are decimal-looking slot ids (the protocol enumerates slots 7…11),
    /// NOT hex 16/17 — reading them as 16/17 is the obvious transcription slip and is what
    /// `slot_ids_are_decimal_looking_not_hex_sixteen_seventeen` exists to catch.
    /// Missing from Android. Slot→gesture mapping INFERRED.
    SetGestureDoubleTap = 0x10,
    /// `0x11` — **triple tap** binding. Captured value `'3'` (next track). Missing from
    /// Android. Slot→gesture mapping INFERRED.
    SetGestureTripleTap = 0x11,
    /// `0x14` — factory reset, data `0x00`. CLIENT-ONLY: declared by both reference implementations and by the
    /// PDF, never observed on any wire.
    FactoryReset = 0x14,
    /// `0x17` — battery read. Reply is `<tens><ones><charging>` where the two digits are
    /// ASCII (`0x30 + n`), so 100 % arrives as `3A 30` — a `'9'+1` overflow the firmware
    /// makes no attempt to avoid. Captured: `17 3a 30 01` (100 %, charging) and
    /// `17 34 39 01` (49 %).
    GetBattery = 0x17,
    /// `0x22` — take photo. `0x30` = photo only, `0x31` = photo **+ HD image for AI**
    /// (the AI image then rides BLE as a `52 58` file stream, not Wi-Fi). Both captured.
    TakePhoto = 0x22,
    /// `0x23` — start video, data `0x00`. Captured.
    StartVideo = 0x23,
    /// `0x24` — stop video, data `0x00`. Captured.
    StopVideo = 0x24,
    /// `0x30` — previous/next track, `0x00` prev / `0x01` next. Captured in `client_1`.
    SwitchMusic = 0x30,
    /// `0x31` — pause/play, `0x00` pause / `0x01` play. Captured.
    PlayPause = 0x31,
    /// `0x32` — RELATIVE volume, `0x00` down / `0x01` up.
    ///
    /// CAPTURE-CONFIRMED that this moves the SYSTEM channel only: in capture `client_1`
    /// seven `0x32` writes walked `0x69` index 0 and left media untouched, while the
    /// glasses' own touchpad moves MEDIA. The vendor app never sends `0x32` at all. Use
    /// [`AppCommand::SetVolume`] with an explicit channel for anything media-related.
    Volume = 0x32,
    /// `0x33` — call control, `0x00` hang up / `0x01` answer. CLIENT-ONLY.
    AnswerHangup = 0x33,
    /// `0x34` — manual voice recording, `0x00` stop / `0x01` start. Captured both ways.
    VoiceRecording = 0x34,
    /// `0x36` — pull image by index. **The ack exists; the data path does not.**
    ///
    /// Missing from Android. Absent from all eight captures — this is a DEVICE-PROBE result
    /// (2026-07-28, `PROTOCOL.md` §13b): the firmware replies in ~30 ms
    /// ECHOING the index byte (`36 00`→`36 00`, `36 05`→`36 05`) while bogus opcodes
    /// `0xEE`/`0xDE` get silence, so the dispatch is real — but no `52 58` data ever follows,
    /// and index 5 echoes as readily as 0, so it does not even range-check. Wi-Fi sync
    /// remains the only route to a full-resolution photo. Do NOT build an image-pull path on
    /// this opcode without new evidence.
    PullImage = 0x36,
    /// `0x37` — pull thumbnail status. Missing from Android. DEVICE-PROBE: **no reply at
    /// all**, with inbound logging armed and a bogus-opcode control run alongside. The
    /// vendor SDK names it `appPullThumbnailImageStatus(int)`; this firmware does not
    /// implement it.
    PullThumbnailStatus = 0x37,
    /// `0x39` — raise the SoftAP serving the **file API** (`/app/getfilelist` …).
    /// `0x30` AP mode, `0x31` direct/p2p.
    ///
    /// Captured in `eyevue_1`, `eyevue_4`, `client_2`. Same SSID, same `0x25` reply ~2.5 s
    /// later and same `0x44` teardown as [`AppCommand::OpenWifiLive`] — the device just
    /// serves something different. The gallery path sends `0x39` and never `0x67`.
    OpenWifiFiles = 0x39,
    /// `0x40` — how many files are waiting. Replies via `0x42`, never echoes `0x40`.
    /// Captured.
    GetFileCount = 0x40,
    /// `0x44` — end-of-download / ISP power-down. `30 00` = all downloaded (clears the
    /// thumbnail count), `30 01` = power the ISP off WITHOUT touching the count (the
    /// abort/cancel teardown), `31 <n>` = partial, device subtracts `n`.
    ///
    /// The teardown for BOTH Wi-Fi modes. Without it the ISP stays powered and the glasses
    /// tone in SoftAP-waiting mode indefinitely. `30 01` captured in `eyevue_1`/`glassx_1`.
    FileDownloadComplete = 0x44,
    /// `0x45` — read the action-sync state. Reply is the 10-byte frame in
    /// `PROTOCOL.md` §4 (the PDF claims 9 fields; the device sends 10).
    /// Shares its byte with [`DeviceUpload::ActionSync`]. Captured.
    GetDeviceStatus = 0x45,
    /// `0x48` — read the switch/settings state.
    ///
    /// **The firmware does NOT answer with a `0x48` frame.** It emits a BURST of one frame
    /// per setting, each keyed by that setting's own SETTER opcode — see
    /// [`SWITCH_STATE_BURST_ORDER`]. Confirmed identically across `glassx_1`, `eyevue_1`
    /// and all three `client_*` captures. Waiting for a `0x48` reply waits forever.
    GetSwitchStates = 0x48,
    /// `0x55` — firmware versions, reply `bt(3) isp(3) hw(1)`. Captured as
    /// `55 01 04 08 01 03 01 02` = bt V1.4.8 / isp V1.3.1 / hw V2 on every unit.
    GetVersions = 0x55,
    /// `0x56` — interrupt/close the microphone.
    ///
    /// **The only thing that closes the mic.** Measured across every capture with voice in
    /// it: the device never stops streaming `0x46` on its own and never sends `0x99`
    /// (`client_1` ran 79.7 s solid, `client_2` 65.1 s, neither with an app-side `0x56`). The
    /// vendor writes it in PAIRS ~0.5 s apart. Captured.
    InterruptVoice = 0x56,
    /// `0x57` — ask the device to retransmit voice. CLIENT-ONLY, never observed.
    RetransmitVoice = 0x57,
    /// `0x59` — push the phone's wall clock as `YY MM DD HH MM SS`, **plain hex, not BCD**
    /// (`1a 07 1b 12 28 12` = 2026-07-27 18:40:18). Captured in five traces, echoed back
    /// verbatim.
    ///
    /// This crate builds the payload from fields the shell supplies; it never reads a clock.
    SendPhoneTime = 0x59,
    /// `0x60` — reboot, data `0x00`. CLIENT-ONLY, never observed.
    Reboot = 0x60,
    /// `0x61` — capture orientation, `0x30` portrait / `0x31` landscape.
    /// Confirmed only as an inbound burst frame (`ac 55 00 03 61 30`).
    Orientation = 0x61,
    /// `0x62` — offline voice language, `0x00` Chinese / `0x01` English. CLIENT-ONLY.
    /// The `0x48` burst never reports it either, so this firmware may not implement it.
    OfflineVoiceLang = 0x62,
    /// `0x63` — enter OTA upgrade mode, `0x30` AP / `0x31` p2p. CLIENT-ONLY.
    EnterUpgrade = 0x63,
    /// `0x64` — project/customer name. Reply is 8 ASCII bytes, 4 project + 4 customer:
    /// captured as `54 31 00 00 30 33 30 33` = `"T1"` / `"0303"`.
    GetProjectName = 0x64,
    /// `0x65` — report the just-flashed ISP version (3 bytes) after an OTA. CLIENT-ONLY.
    SendIspVersion = 0x65,
    /// `0x67` — raise the SoftAP serving the **RTSP live stream** (`rtsp://192.168.169.1:554/h264`).
    /// Same `0x30` AP / `0x31` p2p data byte as [`AppCommand::OpenWifiFiles`].
    ///
    /// Missing from Android. CAPTURE-CONFIRMED in capture `glassx_1`
    /// (`ab 55 00 03 67 30 97`, ack `ac 55 00 03 67 01 68`): GlassX sends `0x67` and NEVER
    /// `0x39` when entering live mode. The vendor APK independently labels command `103`
    /// (= `0x67`) *live*.
    OpenWifiLive = 0x67,
    /// `0x69` — read all three output levels back. Reply is `69 <system> <media> <call>`.
    ///
    /// Missing from Android. CAPTURE-CONFIRMED in `eyevue_1`: three `0x70` writes of
    /// `06`/`07`/`06` were followed by a `0x69` reply of exactly `06 07 06`, which is what
    /// pins the reply ORDER as well as the opcode. Shares its byte with
    /// [`DeviceUpload::Volumes`].
    GetVolumes = 0x69,
    /// `0x70` — set ONE output level: `70 <channel> <level>` ([`VolumeChannel`]). There is no
    /// combined form; the vendor writes three separate commands.
    ///
    /// Missing from Android. CAPTURE-CONFIRMED: `70 01 07`, `70 00 06`, `70 02 06` in
    /// `eyevue_1`, more in `eyevue_3`/`glassx_1`. `level` is carried as a raw byte because
    /// its scale is not pinned — observed `0x05`…`0x10`.
    SetVolume = 0x70,
    /// `0x71` — which voice features are currently DISABLED, as a 1-byte bitmap.
    ///
    /// Missing from Android. CAPTURE-CONFIRMED as a write in `eyevue_1`
    /// (`ab 55 00 03 71 00 71`, echoed `ac 55 00 03 71 00 71`); the vendor's own decoded log
    /// expands it to `{ aiAwaken, offlineVoice }`. Sent in the connect/refresh group between
    /// `0x48` and `0x55`. Only ever observed as `0x00` (nothing disabled), so the BIT
    /// POSITIONS are INFERRED and this crate does not decode them.
    GetVoiceDisableState = 0x71,
    /// `0x95` — device capability bitmap, 4 bytes BIG-endian.
    ///
    /// Missing from Android. CAPTURE-CONFIRMED as `00 00 04 0B` on every unit in every
    /// capture (`eyevue_1`, `eyevue_3`, `glassx_1`, `client_1`, `client_2`, `client_3`). Four bits
    /// set, and the vendor app shows exactly four true flags — but it prints its map
    /// alphabetically, so the bit↔flag assignment cannot be recovered from one value and is
    /// NOT pinned. Treat the word as an opaque hardware fingerprint.
    ///
    /// The device also PUSHES this unsolicited ~30 ms after every LE link-up, before the app
    /// writes anything (the CCCD survives the bond) — so a `0x95` reply can arrive with no
    /// request outstanding, and correlating one against an in-flight command is a bug.
    GetCapabilities = 0x95,
}

impl AppCommand {
    /// Every app→device opcode, ascending by wire byte.
    pub const ALL: [AppCommand; 41] = [
        AppCommand::SetLed,
        AppCommand::SetRecordDuration,
        AppCommand::WearDetection,
        AppCommand::VoiceCommand,
        AppCommand::SetGestureSwipeForward,
        AppCommand::SetGestureSwipeBack,
        AppCommand::SetGestureSingleTap,
        AppCommand::SetGestureDoubleTap,
        AppCommand::SetGestureTripleTap,
        AppCommand::FactoryReset,
        AppCommand::GetBattery,
        AppCommand::TakePhoto,
        AppCommand::StartVideo,
        AppCommand::StopVideo,
        AppCommand::SwitchMusic,
        AppCommand::PlayPause,
        AppCommand::Volume,
        AppCommand::AnswerHangup,
        AppCommand::VoiceRecording,
        AppCommand::PullImage,
        AppCommand::PullThumbnailStatus,
        AppCommand::OpenWifiFiles,
        AppCommand::GetFileCount,
        AppCommand::FileDownloadComplete,
        AppCommand::GetDeviceStatus,
        AppCommand::GetSwitchStates,
        AppCommand::GetVersions,
        AppCommand::InterruptVoice,
        AppCommand::RetransmitVoice,
        AppCommand::SendPhoneTime,
        AppCommand::Reboot,
        AppCommand::Orientation,
        AppCommand::OfflineVoiceLang,
        AppCommand::EnterUpgrade,
        AppCommand::GetProjectName,
        AppCommand::SendIspVersion,
        AppCommand::OpenWifiLive,
        AppCommand::GetVolumes,
        AppCommand::SetVolume,
        AppCommand::GetVoiceDisableState,
        AppCommand::GetCapabilities,
    ];

    /// The wire byte.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Classify an outbound byte, or `None` if it is not one we know.
    ///
    /// `None` is a real answer, not a failure: the encoder accepts a raw byte
    /// ([`crate::frame::encode`]) precisely so an unknown opcode can be probed
    /// without first being enshrined here.
    pub fn from_code(code: u8) -> Option<AppCommand> {
        AppCommand::ALL.into_iter().find(|c| c.code() == code)
    }

    /// What backs this opcode. Total by construction — adding a variant without an evidence
    /// arm does not compile.
    pub fn evidence(self) -> Evidence {
        use AppCommand::*;
        match self {
            // Seen on the wire in the packet captures, in at least one direction.
            SetLed
            | SetRecordDuration
            | WearDetection
            | VoiceCommand
            | SetGestureSwipeForward
            | SetGestureSwipeBack
            | SetGestureSingleTap
            | SetGestureDoubleTap
            | SetGestureTripleTap
            | GetBattery
            | TakePhoto
            | StartVideo
            | StopVideo
            | SwitchMusic
            | PlayPause
            | Volume
            | VoiceRecording
            | OpenWifiFiles
            | GetFileCount
            | FileDownloadComplete
            | GetDeviceStatus
            | GetSwitchStates
            | GetVersions
            | InterruptVoice
            | SendPhoneTime
            | Orientation
            | GetProjectName
            | OpenWifiLive
            | GetVolumes
            | SetVolume
            | GetVoiceDisableState
            | GetCapabilities => Evidence::Capture,

            // Absent from every capture; characterised against hardware on 2026-07-28 with
            // inbound logging armed and a bogus-opcode control (§13b).
            PullImage | PullThumbnailStatus => Evidence::DeviceProbe,

            // Declared by a client and/or the v07 PDF, never observed anywhere.
            FactoryReset | AnswerHangup | RetransmitVoice | Reboot | OfflineVoiceLang
            | EnterUpgrade | SendIspVersion => Evidence::ClientOnly,
        }
    }
}

/// Device → app frame bytes (protocol §2.2.2) that are NOT an echo of a command.
///
/// The inbound stream also carries verbatim echoes of most setters — `0x22`, `0x34`, `0x39`,
/// `0x55`, `0x59`, `0x64`, `0x67`, `0x71`, `0x95` and the whole `0x48` burst are all confirmed
/// arriving with an [`AppCommand`] byte in the `cmd` field. This enum is only the set that has
/// no outbound counterpart, so `DeviceUpload::from_code` returning `None` never means
/// "corrupt".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum DeviceUpload {
    /// `0x25` — SoftAP SSID, NUL-terminated ASCII. The password is the fixed `12345678`.
    /// Captured as `"DH-TwI-4641E4"` in `eyevue_1`, `eyevue_4`, `glassx_1`, `client_2`,
    /// arriving ~2.5 s after `0x39`/`0x67`.
    WifiName = 0x25,
    /// `0x42` — thumbnail/file count, 2 bytes BIG-endian. Captured across `0x000a`…`0x0013`.
    ThumbnailCount = 0x42,
    /// `0x45` — action sync, 10 bytes, one flag per action. The PDF says 9; the device sends
    /// 10. Index 9 is file-IMPORT mode (goes 1 while a Wi-Fi session is open), not "Wi-Fi
    /// active". Indices 5, 6 and 8 (nod / shake / wearing) have never fired in any capture.
    /// Shares its byte with [`AppCommand::GetDeviceStatus`].
    ActionSync = 0x45,
    /// `0x46` — voice PCM, ~50 frames/second, 40 payload bytes per frame in every capture
    /// (`ac 55 00 2a 46 …`). 15,085 of the captured frames are these.
    VoiceData = 0x46,
    /// `0x49` — abandon voice. CLIENT-ONLY, never observed.
    AbandonVoice = 0x49,
    /// `0x51` — cancel AI broadcast. CLIENT-ONLY, never observed.
    CancelAiBroadcast = 0x51,
    /// `0x52` — HD image capture failed. CLIENT-ONLY, never observed.
    ///
    /// Not to be confused with the `52 58` file-frame magic, which is a different framing
    /// entirely and lives on `AA15`.
    HdImageFailed = 0x52,
    /// `0x53` — unsolicited battery push, `<charging> <percent>`. Percent is a RAW byte here
    /// (`53 01 31` = charging, 49 %), unlike `0x17`'s ASCII digit pair. Pushed roughly every
    /// 10 s discharging / 60 s charging. Captured in `eyevue_1`, `eyevue_2`, `glassx_1`.
    ChargeBattery = 0x53,
    /// `0x69` — reply to [`AppCommand::GetVolumes`]: three levels in `[system, media, call]`
    /// order. The ORDER is capture-pinned, see that command. Missing from the reference Android implementation.
    Volumes = 0x69,
    /// `0x96` — ISP upgrade finished. CLIENT-ONLY, never observed.
    IspUpgradeDone = 0x96,
    /// `0x97` — wake-word voice capture started, data `0x01`. Captured (`ac 55 00 03 97 01 98`).
    VoiceUploadStart = 0x97,
    /// `0x99` — voice capture ended.
    ///
    /// **Never sent by this firmware** — do not wait for it. Four captures totalling 14,636
    /// `0x46` frames contain zero `0x99`. The mic closes when the app writes
    /// [`AppCommand::InterruptVoice`] and at no other time; an end-of-utterance signal has to
    /// come from an idle timer the shell owns.
    VoiceUploadEnd = 0x99,
}

impl DeviceUpload {
    /// Every device→app push opcode, ascending by wire byte.
    pub const ALL: [DeviceUpload; 12] = [
        DeviceUpload::WifiName,
        DeviceUpload::ThumbnailCount,
        DeviceUpload::ActionSync,
        DeviceUpload::VoiceData,
        DeviceUpload::AbandonVoice,
        DeviceUpload::CancelAiBroadcast,
        DeviceUpload::HdImageFailed,
        DeviceUpload::ChargeBattery,
        DeviceUpload::Volumes,
        DeviceUpload::IspUpgradeDone,
        DeviceUpload::VoiceUploadStart,
        DeviceUpload::VoiceUploadEnd,
    ];

    /// The wire byte.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Classify an inbound byte, or `None` — which usually means "an echo of a setter",
    /// not "corrupt". See the type docs.
    pub fn from_code(code: u8) -> Option<DeviceUpload> {
        DeviceUpload::ALL.into_iter().find(|c| c.code() == code)
    }

    /// What backs this opcode.
    pub fn evidence(self) -> Evidence {
        use DeviceUpload::*;
        match self {
            WifiName | ThumbnailCount | ActionSync | VoiceData | ChargeBattery | Volumes
            | VoiceUploadStart => Evidence::Capture,
            // Declared by both reference implementations and the PDF; no capture, no probe. `VoiceUploadEnd` is
            // the one that matters — a session that waits for it hangs.
            AbandonVoice | CancelAiBroadcast | HdImageFailed | IspUpgradeDone | VoiceUploadEnd => {
                Evidence::ClientOnly
            }
        }
    }
}

/// The five touchpad gesture SLOTS reported in the `0x48` burst.
///
/// The slot IDS are CAPTURE-CONFIRMED (all five frames appear in six captures). The
/// slot→GESTURE mapping below is **INFERRED**: the five values are `0 1 2 4 3` on every unit,
/// which matches the factory default printed on p.2 of the protocol PDF exactly, and that
/// self-consistency is the entire basis. No capture has ever CHANGED a binding, so nobody has
/// watched a specific slot move. To settle it, change one gesture in the vendor app and see
/// which slot's value changes (`PROTOCOL.md` §5).
///
/// Absent from the reference Android implementation entirely — this enum is the touch-input surface Android has
/// never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum GestureSlot {
    /// `0x07`. INFERRED: swipe forward. Captured default `'0'` (volume down).
    SwipeForward = 0x07,
    /// `0x08`. INFERRED: swipe back. Captured default `'1'` (volume up).
    SwipeBack = 0x08,
    /// `0x09`. INFERRED: single tap. Captured default `'2'` (play/pause).
    SingleTap = 0x09,
    /// `0x10`. INFERRED: double tap. Captured default `'4'` (previous track).
    DoubleTap = 0x10,
    /// `0x11`. INFERRED: triple tap. Captured default `'3'` (next track).
    TripleTap = 0x11,
}

impl GestureSlot {
    /// All five slots, in the order the burst emits them.
    pub const ALL: [GestureSlot; 5] = [
        GestureSlot::SwipeForward,
        GestureSlot::SwipeBack,
        GestureSlot::SingleTap,
        GestureSlot::DoubleTap,
        GestureSlot::TripleTap,
    ];

    /// The ASCII-digit value each slot carries on a factory-default unit, in [`Self::ALL`]
    /// order: `'0' '1' '2' '4' '3'`. CAPTURE-CONFIRMED as a byte sequence on six units; what
    /// each value MEANS is the inference.
    pub const CAPTURED_DEFAULTS: [u8; 5] = *b"01243";

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<GestureSlot> {
        GestureSlot::ALL.into_iter().find(|g| g.code() == code)
    }

    /// The setter command that carries this slot. Same byte — stated as a function so the
    /// identity is checked rather than assumed.
    pub fn command(self) -> AppCommand {
        match self {
            GestureSlot::SwipeForward => AppCommand::SetGestureSwipeForward,
            GestureSlot::SwipeBack => AppCommand::SetGestureSwipeBack,
            GestureSlot::SingleTap => AppCommand::SetGestureSingleTap,
            GestureSlot::DoubleTap => AppCommand::SetGestureDoubleTap,
            GestureSlot::TripleTap => AppCommand::SetGestureTripleTap,
        }
    }
}

/// The action a gesture slot is bound to — the slot frame's value byte, as an ASCII digit.
///
/// Same INFERRED confidence as [`GestureSlot`]: the numbering comes from the PDF's default
/// table, which the captured values match, and from nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum GestureAction {
    VolumeDown = 0,
    VolumeUp = 1,
    PlayPause = 2,
    NextTrack = 3,
    PreviousTrack = 4,
}

impl GestureAction {
    pub const ALL: [GestureAction; 5] = [
        GestureAction::VolumeDown,
        GestureAction::VolumeUp,
        GestureAction::PlayPause,
        GestureAction::NextTrack,
        GestureAction::PreviousTrack,
    ];

    /// The ordinal, i.e. the digit's numeric value — NOT the wire byte.
    pub fn ordinal(self) -> u8 {
        self as u8
    }

    /// The ASCII byte that appears in a slot frame (`0x30 + ordinal`).
    pub fn ascii(self) -> u8 {
        b'0' + self.ordinal()
    }

    /// Decode a slot frame's value byte.
    ///
    /// Accepts BOTH the ASCII digit every captured firmware sends and a raw `0..=4`, so a
    /// variant that omits the ASCII offset still decodes. Anything else is `None` rather than
    /// a silent clamp — an out-of-range binding must not surface as "volume down".
    pub fn from_value(byte: u8) -> Option<GestureAction> {
        let ordinal = if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            byte
        };
        GestureAction::ALL.into_iter().find(|a| a.ordinal() == ordinal)
    }
}

/// The three independently-addressable output levels: the channel byte of
/// [`AppCommand::SetVolume`] and the reply ORDER of [`AppCommand::GetVolumes`].
///
/// CAPTURE-CONFIRMED in capture `eyevue_1`: the vendor app's media / system / call sliders
/// emitted `70 01 07`, `70 00 06`, `70 02 06`, and the following `0x69` read back exactly
/// `06 07 06`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum VolumeChannel {
    System = 0x00,
    Media = 0x01,
    Call = 0x02,
}

impl VolumeChannel {
    /// In the order a `0x69` reply lists them.
    pub const ALL: [VolumeChannel; 3] = [
        VolumeChannel::System,
        VolumeChannel::Media,
        VolumeChannel::Call,
    ];

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<VolumeChannel> {
        VolumeChannel::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// The exact order of the `0x48` switch-state burst, as ten separate frames.
///
/// CAPTURE-CONFIRMED byte-for-byte in capture `glassx_1` and capture `eyevue_1`, and the
/// same order in all three `client_*` traces:
///
/// ```text
/// ac 55 00 03 01 31      LED           = '1'
/// ac 55 00 04 02 00 3c   record secs   = 0x003C (60)  ← 2-byte BE, NOT ASCII
/// ac 55 00 03 04 31      wear detect   = '1'
/// ac 55 00 03 06 31      voice command = '1'
/// ac 55 00 03 07 30      gesture slot  ┐
/// ac 55 00 03 08 31      gesture slot  │
/// ac 55 00 03 09 32      gesture slot  │ five slots
/// ac 55 00 03 10 34      gesture slot  │
/// ac 55 00 03 11 33      gesture slot  ┘
/// ac 55 00 03 61 30      orientation   = '0'
/// ```
///
/// The PDF's item 7 (offline voice language, `0x62`) is **never reported** by this firmware,
/// which is why a burst accumulator must not require it to consider itself complete.
pub const SWITCH_STATE_BURST_ORDER: [AppCommand; 10] = [
    AppCommand::SetLed,
    AppCommand::SetRecordDuration,
    AppCommand::WearDetection,
    AppCommand::VoiceCommand,
    AppCommand::SetGestureSwipeForward,
    AppCommand::SetGestureSwipeBack,
    AppCommand::SetGestureSingleTap,
    AppCommand::SetGestureDoubleTap,
    AppCommand::SetGestureTripleTap,
    AppCommand::Orientation,
];

/// Opcodes the vendor's Android SDK NAMES but which neither client implements and no capture
/// has produced. Deliberately NOT enum variants — a name in a decompiled jump table is not
/// permission to send a byte at hardware.
///
/// Recorded so a future session does not re-derive them from `z39.java`
/// (`PROTOCOL.md` §13a). Promote one to [`AppCommand`] when, and only when,
/// a probe or capture backs it.
pub const VENDOR_NAMED_UNIMPLEMENTED: [(u8, &str); 5] = [
    (0x03, "microphone"),
    (0x16, "capacity"),
    (0x35, "camera style"),
    (0x43, "gesture recover"),
    (0x50, "camera mode"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The union is pinned EXACTLY, by byte, not by spot-check. The entire justification for
    /// this crate owning the glasses protocol is that the two clients' opcode sets stop being
    /// able to drift; a set that can grow or shrink without a test failing gives that up.
    #[test]
    fn the_app_command_set_is_exactly_the_forty_one_union_bytes() {
        let mut got: Vec<u8> = AppCommand::ALL.iter().map(|c| c.code()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                0x01, 0x02, 0x04, 0x06, 0x07, 0x08, 0x09, 0x10, 0x11, 0x14, 0x17, 0x22, 0x23,
                0x24, 0x30, 0x31, 0x32, 0x33, 0x34, 0x36, 0x37, 0x39, 0x40, 0x44, 0x45, 0x48,
                0x55, 0x56, 0x57, 0x59, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x67, 0x69, 0x70,
                0x71, 0x95,
            ]
        );
        // No duplicate discriminants — two names on one byte would make `from_code` pick
        // whichever came first in the array, silently.
        let mut dedup = got.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), AppCommand::ALL.len());
    }

    #[test]
    fn the_device_upload_set_is_exactly_the_twelve_push_bytes() {
        let mut got: Vec<u8> = DeviceUpload::ALL.iter().map(|c| c.code()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![0x25, 0x42, 0x45, 0x46, 0x49, 0x51, 0x52, 0x53, 0x69, 0x96, 0x97, 0x99]
        );
    }

    /// `0x10` and `0x11` are slot ids 10 and 11 in a decimal-looking sequence that starts at
    /// `0x07` — they are NOT hex 16 and 17. Reading them as 16/17 is the transcription slip
    /// this protocol invites, and it would bind double-tap to a byte the device never sends.
    #[test]
    fn slot_ids_are_decimal_looking_not_hex_sixteen_seventeen() {
        assert_eq!(GestureSlot::DoubleTap.code(), 0x10);
        assert_eq!(GestureSlot::TripleTap.code(), 0x11);
        // The five ids are contiguous as *slots* 7…11, so the run is 07 08 09 10 11.
        let codes: Vec<u8> = GestureSlot::ALL.iter().map(|g| g.code()).collect();
        assert_eq!(codes, vec![0x07, 0x08, 0x09, 0x10, 0x11]);
        assert!(!codes.contains(&0x16) && !codes.contains(&0x17));
        // 0x17 is the battery read — the collision a hex reading would cause.
        assert_eq!(AppCommand::GetBattery.code(), 0x17);
    }

    /// Slot ids and their setter commands are the same byte. Stated as a test because
    /// `GestureSlot::command` is the seam a burst accumulator will route through.
    #[test]
    fn gesture_slots_and_their_setter_commands_share_a_byte() {
        for slot in GestureSlot::ALL {
            assert_eq!(slot.code(), slot.command().code());
            assert_eq!(AppCommand::from_code(slot.code()), Some(slot.command()));
        }
    }

    /// The captured factory-default bindings, in burst order. If a future capture reads
    /// anything else, either the unit is not at defaults or the slot mapping is wrong — and
    /// this is where that shows up.
    #[test]
    fn captured_gesture_defaults_are_zero_one_two_four_three() {
        assert_eq!(GestureSlot::CAPTURED_DEFAULTS, *b"01243");
        let decoded: Vec<GestureAction> = GestureSlot::CAPTURED_DEFAULTS
            .iter()
            .map(|b| GestureAction::from_value(*b).expect("captured default is in range"))
            .collect();
        assert_eq!(
            decoded,
            vec![
                GestureAction::VolumeDown,
                GestureAction::VolumeUp,
                GestureAction::PlayPause,
                GestureAction::PreviousTrack,
                GestureAction::NextTrack,
            ]
        );
    }

    /// ASCII is what every captured firmware sends; a raw ordinal is accepted as a variant
    /// tolerance. An out-of-range byte is `None`, never a clamp — clamping would report a
    /// corrupt binding as "volume down" and the user would see a plausible lie.
    #[test]
    fn gesture_action_decodes_ascii_and_raw_but_never_clamps() {
        assert_eq!(GestureAction::from_value(b'2'), Some(GestureAction::PlayPause));
        assert_eq!(GestureAction::from_value(2), Some(GestureAction::PlayPause));
        assert_eq!(GestureAction::PlayPause.ascii(), 0x32);
        assert_eq!(GestureAction::from_value(b'5'), None);
        assert_eq!(GestureAction::from_value(0xFF), None);
    }

    /// The burst is ten frames in a fixed order, and `0x62` is not one of them. An
    /// accumulator that waits for an offline-voice-language frame never completes.
    #[test]
    fn the_switch_state_burst_is_ten_frames_and_excludes_offline_voice_lang() {
        assert_eq!(SWITCH_STATE_BURST_ORDER.len(), 10);
        assert_eq!(SWITCH_STATE_BURST_ORDER[0], AppCommand::SetLed);
        assert_eq!(SWITCH_STATE_BURST_ORDER[9], AppCommand::Orientation);
        assert!(!SWITCH_STATE_BURST_ORDER.contains(&AppCommand::OfflineVoiceLang));
        // The five gesture slots sit in the middle, contiguous, in slot order.
        assert_eq!(
            &SWITCH_STATE_BURST_ORDER[4..9],
            &GestureSlot::ALL.map(GestureSlot::command)
        );
    }

    /// Two bytes are shared across directions. A router that assumes `cmd` identifies the
    /// direction gets `0x45` and `0x69` wrong, and `0x69` is the one that matters — a
    /// volume READ reply looks exactly like a volume read REQUEST on the byte alone.
    #[test]
    fn zero_x45_and_zero_x69_are_deliberately_shared_across_directions() {
        assert_eq!(AppCommand::GetDeviceStatus.code(), DeviceUpload::ActionSync.code());
        assert_eq!(AppCommand::GetVolumes.code(), DeviceUpload::Volumes.code());
        let shared: Vec<u8> = AppCommand::ALL
            .iter()
            .map(|c| c.code())
            .filter(|c| DeviceUpload::from_code(*c).is_some())
            .collect();
        assert_eq!(shared, vec![0x45, 0x69], "exactly two bytes are two-way");
    }

    /// Evidence levels are asserted for the opcodes whose level changes what a caller may do.
    /// Rounding one upward is the specific failure this project has been burned by.
    #[test]
    fn evidence_levels_do_not_round_upward() {
        // Never observed anywhere: sending these is a guess.
        for c in [
            AppCommand::FactoryReset,
            AppCommand::Reboot,
            AppCommand::EnterUpgrade,
            AppCommand::OfflineVoiceLang,
        ] {
            assert_eq!(c.evidence(), Evidence::ClientOnly, "{c:?}");
        }
        // Probed against hardware, absent from every capture. `PullImage` acks and delivers
        // nothing; `PullThumbnailStatus` is silent. Neither is Capture.
        assert_eq!(AppCommand::PullImage.evidence(), Evidence::DeviceProbe);
        assert_eq!(AppCommand::PullThumbnailStatus.evidence(), Evidence::DeviceProbe);
        // The twelve Android-missing opcodes are not speculative — ten of them are on the
        // wire, which is why the gap is a real regression rather than an unused surface.
        assert_eq!(AppCommand::GetCapabilities.evidence(), Evidence::Capture);
        assert_eq!(AppCommand::OpenWifiLive.evidence(), Evidence::Capture);
        assert_eq!(AppCommand::SetVolume.evidence(), Evidence::Capture);
        // A session that waits for 0x99 hangs; the level says so.
        assert_eq!(DeviceUpload::VoiceUploadEnd.evidence(), Evidence::ClientOnly);
        assert_eq!(DeviceUpload::VoiceData.evidence(), Evidence::Capture);
    }

    /// The census the module docs quote, kept executable. A prose count is exactly the kind of
    /// claim that rots the first time somebody adds an opcode without thinking about evidence.
    #[test]
    fn the_evidence_census_matches_the_documented_counts() {
        let count = |want: Evidence| {
            AppCommand::ALL.iter().filter(|c| c.evidence() == want).count()
                + DeviceUpload::ALL.iter().filter(|u| u.evidence() == want).count()
        };
        assert_eq!(count(Evidence::Capture), 39);
        assert_eq!(count(Evidence::DeviceProbe), 2);
        assert_eq!(count(Evidence::ClientOnly), 12);
        assert_eq!(AppCommand::ALL.len() + DeviceUpload::ALL.len(), 53);
    }

    /// Vendor-named bytes stay OUT of the enum. `from_code` must not recognise one, or a
    /// future caller reads recognition as permission to send it.
    #[test]
    fn vendor_named_unimplemented_opcodes_are_not_recognised() {
        for (code, name) in VENDOR_NAMED_UNIMPLEMENTED {
            assert_eq!(
                AppCommand::from_code(code),
                None,
                "{name} ({code:#04x}) is a name in a decompiled table, not a command"
            );
        }
    }

    #[test]
    fn from_code_round_trips_every_variant_and_rejects_the_probe_controls() {
        for c in AppCommand::ALL {
            assert_eq!(AppCommand::from_code(c.code()), Some(c));
        }
        for u in DeviceUpload::ALL {
            assert_eq!(DeviceUpload::from_code(u.code()), Some(u));
        }
        // 0xEE / 0xDE are the bogus-opcode controls the §13b probe used; the firmware drops
        // them silently, and so must we.
        assert_eq!(AppCommand::from_code(0xEE), None);
        assert_eq!(AppCommand::from_code(0xDE), None);
    }

    #[test]
    fn volume_channels_are_pinned_to_the_capture_order() {
        assert_eq!(VolumeChannel::System.code(), 0x00);
        assert_eq!(VolumeChannel::Media.code(), 0x01);
        assert_eq!(VolumeChannel::Call.code(), 0x02);
        assert_eq!(VolumeChannel::from_code(0x03), None);
    }
}
