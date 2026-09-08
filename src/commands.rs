//! App → device command builders: the PAYLOAD conventions, once.
//!
//! [`crate::frame::encode_command`] owns the envelope. This module owns the bytes inside it,
//! and it exists because those bytes are not consistent and both reference implementations hand-carry their own
//! table of them:
//!
//! | Family | Encoding | Example |
//! |---|---|---|
//! | Settings (`0x01`, `0x04`, `0x06`, `0x61`, `0x39`, `0x63`) | ASCII digits | on = `0x31`, off = `0x30` |
//! | Media / call (`0x30`, `0x31`, `0x32`, `0x33`, `0x34`) | RAW 0/1 | play = `0x01`, pause = `0x00` |
//! | Record duration (`0x02`) | 2-byte BIG-endian integer | 60 s = `00 3C` |
//! | Volume (`0x70`) | raw channel + raw level | `70 01 07` |
//! | Phone time (`0x59`) | plain hex, NOT BCD | 2026-07-27 = `1A 07 1B …` |
//!
//! Two of those look alike and are not: `voice_command(true)` sends `0x31` and
//! `play_pause(true)` sends `0x01`. Getting them backwards produces a well-formed frame the
//! firmware drops in silence, which is the failure mode with nothing in any log.
//!
//! ## Evidence
//!
//! The OPCODES carry their own evidence level ([`AppCommand::evidence`]). The payload
//! conventions here come from three places and each builder says which: a capture (`0x22`,
//! `0x39`, `0x67`, `0x70`, `0x01`, `0x02`, `0x59` and the gesture setters are all observed on
//! the wire), the v07 PDF, or both shipping clients agreeing. Where only the clients agree, the
//! builder says so — a convention two reference implementations share is not a device confirmation, it is one
//! guess written twice.
//!
//! ## No clock, no defaults
//!
//! [`send_phone_time`] takes components. Both reference implementations call `Date()` / `Calendar.getInstance()`
//! inside the builder, which is exactly the shape that let Android stamp a value against the
//! phone's *current* zone. This crate cannot read a clock and this
//! signature makes the shell's zone choice visible at the call site.

use crate::frame::encode_command;
use crate::opcodes::{AppCommand, GestureAction, GestureSlot, VolumeChannel};
use crate::parser::{Orientation, WifiService};

/// ASCII `'1'` / `'0'` — the settings-family boolean.
fn ascii_flag(on: bool) -> u8 {
    if on {
        b'1'
    } else {
        b'0'
    }
}

/// Raw `0x01` / `0x00` — the media/call-family boolean. Deliberately a separate function from
/// [`ascii_flag`] so the two conventions cannot be reached through one name.
fn raw_flag(on: bool) -> u8 {
    u8::from(on)
}

/// Indicator-LED brightness. There is no "off" level; the PDF's range is three values and all
/// three are captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LedLevel {
    /// Wire `'0'`.
    Low = b'0',
    /// Wire `'1'`. The value every captured unit reports.
    Mid = b'1',
    /// Wire `'2'`.
    High = b'2',
}

impl LedLevel {
    pub const ALL: [LedLevel; 3] = [LedLevel::Low, LedLevel::Mid, LedLevel::High];

    /// The ASCII byte that goes on the wire.
    pub fn ascii(self) -> u8 {
        self as u8
    }

    /// From the `0..=2` level a `0x01` burst frame decodes to
    /// ([`SwitchReport::Led`](crate::parser::SwitchReport::Led)). `None` above 2 rather than a
    /// clamp — a clamp would push an out-of-range setting BACK to the hardware as a plausible
    /// one, and on iOS the reported level round-trips through `@AppStorage` to the device.
    pub fn from_level(level: u8) -> Option<LedLevel> {
        LedLevel::ALL
            .into_iter()
            .find(|l| l.ascii() - b'0' == level)
    }
}

// ---------------------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------------------

/// `0x22` — take a photo. `for_ai` adds the full-resolution still the AI path wants
/// (`0x31`), against `0x30` for a plain photo. CAPTURE-CONFIRMED in both directions.
pub fn take_photo(for_ai: bool) -> Vec<u8> {
    let byte = if for_ai { 0x31 } else { 0x30 };
    encode_command(AppCommand::TakePhoto, &[byte])
}

/// `0x23` — start a video recording. Payload is the protocol's `0x00` filler.
pub fn start_video() -> Vec<u8> {
    encode_command(AppCommand::StartVideo, &[])
}

/// `0x24` — stop it.
pub fn stop_video() -> Vec<u8> {
    encode_command(AppCommand::StopVideo, &[])
}

/// `0x34` — start / stop an on-device audio recording. RAW `0x01`/`0x00`, from both clients.
pub fn voice_recording(start: bool) -> Vec<u8> {
    encode_command(AppCommand::VoiceRecording, &[raw_flag(start)])
}

/// `0x40` — how many photos/videos the device is holding. The reply is a `0x42`, not a `0x40`.
pub fn get_file_count() -> Vec<u8> {
    encode_command(AppCommand::GetFileCount, &[])
}

// ---------------------------------------------------------------------------------------
// Wi-Fi
// ---------------------------------------------------------------------------------------

/// `0x39` (files) / `0x67` (live) — raise the SoftAP. `p2p` selects `0x31` over `0x30`.
///
/// One function over [`WifiService`] rather than two, because the pair is the same command with
/// a different byte and the reference Android implementation implements only one of them — a split signature is how
/// that gap stayed invisible. The SSID arrives separately in a `0x25` about 2.5 s later; the
/// passphrase is not on the wire (see
/// [`WifiCredentials::PASSPHRASE`](crate::parser::WifiCredentials::PASSPHRASE)).
pub fn open_wifi(service: WifiService, p2p: bool) -> Vec<u8> {
    let byte = if p2p { 0x31 } else { 0x30 };
    encode_command(service.command(), &[byte])
}

/// `0x44 30 00` — everything transferred, clear the thumbnail count.
pub fn file_download_complete() -> Vec<u8> {
    encode_command(AppCommand::FileDownloadComplete, &[0x30, 0x00])
}

/// `0x44 31 <n>` — partial: the device subtracts `downloaded` from its own count, powers the
/// ISP off and re-reports the remainder.
pub fn file_download_partial(downloaded: u8) -> Vec<u8> {
    encode_command(AppCommand::FileDownloadComplete, &[0x31, downloaded])
}

/// `0x44 30 01` — power the ISP back off WITHOUT touching the thumbnail count.
///
/// The teardown for a Wi-Fi session that downloaded nothing: abort, cancel, navigate-away,
/// error. Skipping it leaves the ISP powered and the glasses toning in SoftAP-waiting mode
/// indefinitely, which is the "constant command-reject tone" both clients hit.
pub fn power_off_isp() -> Vec<u8> {
    encode_command(AppCommand::FileDownloadComplete, &[0x30, 0x01])
}

// ---------------------------------------------------------------------------------------
// Media and call — RAW booleans, not ASCII
// ---------------------------------------------------------------------------------------

/// `0x30` — previous (`0x00`) / next (`0x01`) track.
pub fn switch_music(next: bool) -> Vec<u8> {
    encode_command(AppCommand::SwitchMusic, &[raw_flag(next)])
}

/// `0x31` — play (`0x01`) / pause (`0x00`).
pub fn play_pause(play: bool) -> Vec<u8> {
    encode_command(AppCommand::PlayPause, &[raw_flag(play)])
}

/// `0x32` — volume up (`0x01`) / down (`0x00`), the STEP command.
///
/// Not to be confused with [`set_volume`] (`0x70`), which sets one channel's absolute level.
/// Note also what this is NOT: the inbound `ac 55 … 32 …` frames in the captures are the
/// device ECHOING this write back (each is preceded ~25 ms by the `ab 55` request), not a
/// wearer touchpad gesture. There is no inbound volume-gesture path to handle.
pub fn volume_step(up: bool) -> Vec<u8> {
    encode_command(AppCommand::Volume, &[raw_flag(up)])
}

/// `0x33` — answer (`0x01`) / hang up (`0x00`). CLIENT-ONLY opcode: no capture contains one.
pub fn answer_hangup(answer: bool) -> Vec<u8> {
    encode_command(AppCommand::AnswerHangup, &[raw_flag(answer)])
}

/// `0x70` — set ONE output level. There is no combined form; the vendor writes three commands.
///
/// `level` is carried VERBATIM. The scale is not pinned (captured `0x05`…`0x10`, no maximum
/// ever observed), and normalising an unpinned scale is how a slider ends up lying.
pub fn set_volume(channel: VolumeChannel, level: u8) -> Vec<u8> {
    encode_command(AppCommand::SetVolume, &[channel.code(), level])
}

/// `0x69` — read all three levels back. Reply order is `[system, media, call]`.
pub fn get_volumes() -> Vec<u8> {
    encode_command(AppCommand::GetVolumes, &[])
}

// ---------------------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------------------

/// `0x56` — interrupt the voice transmission. **The only thing that closes the device mic.**
///
/// Four captures totalling 14,636 `0x46` frames contain no `0x99` and no gap, so a session
/// waiting to observe the device closing its own microphone waits forever. `0x49` is a
/// DEVICE → app opcode and writing it does nothing — a bug both clients shipped.
pub fn interrupt_voice() -> Vec<u8> {
    encode_command(AppCommand::InterruptVoice, &[])
}

/// `0x57` — ask for a retransmit. CLIENT-ONLY; never observed in either direction.
pub fn retransmit_voice() -> Vec<u8> {
    encode_command(AppCommand::RetransmitVoice, &[])
}

/// `0x71` — read which voice features are DISABLED. `0x00` back means the wake word is live.
pub fn get_voice_disable_state() -> Vec<u8> {
    encode_command(AppCommand::GetVoiceDisableState, &[])
}

// ---------------------------------------------------------------------------------------
// Settings — ASCII booleans
// ---------------------------------------------------------------------------------------

/// `0x01` — indicator-LED brightness. A dead feature on every client, but a real frame that
/// arrives in every `0x48` burst.
pub fn set_led(level: LedLevel) -> Vec<u8> {
    encode_command(AppCommand::SetLed, &[level.ascii()])
}

/// `0x02` — video/loop record duration in seconds, 2-byte BIG-endian. The one setting whose
/// value is not an ASCII digit. Captured as 60, 90 and 180.
pub fn set_record_duration(seconds: u16) -> Vec<u8> {
    encode_command(
        AppCommand::SetRecordDuration,
        &[(seconds >> 8) as u8, (seconds & 0xFF) as u8],
    )
}

/// `0x04` — wear detection on/off.
pub fn set_wear_detection(on: bool) -> Vec<u8> {
    encode_command(AppCommand::WearDetection, &[ascii_flag(on)])
}

/// `0x06` — voice command / wake word on/off.
pub fn set_voice_command(on: bool) -> Vec<u8> {
    encode_command(AppCommand::VoiceCommand, &[ascii_flag(on)])
}

/// `0x61` — capture orientation.
pub fn set_orientation(orientation: Orientation) -> Vec<u8> {
    let byte = match orientation {
        Orientation::Portrait => b'0',
        Orientation::Landscape => b'1',
    };
    encode_command(AppCommand::Orientation, &[byte])
}

/// `0x62` — offline voice language, RAW `0x00` Chinese / `0x01` English.
///
/// Both clients agree on the raw encoding, which is why this is not an [`ascii_flag`]. The
/// opcode is CLIENT-ONLY and this firmware never reports it in a `0x48` burst, so nothing has
/// confirmed either the byte or the effect.
pub fn set_offline_voice_language(english: bool) -> Vec<u8> {
    encode_command(AppCommand::OfflineVoiceLang, &[raw_flag(english)])
}

/// `0x07`–`0x11` — bind one touchpad gesture slot to an action.
///
/// **Neither shipping client can send this**: iOS declares no gesture setter and Android
/// declares none of the five opcodes at all. The slot ids and their captured values are
/// DEVICE-CONFIRMED; which physical gesture a slot IS remains inferred (see [`GestureSlot`]),
/// so a UI built on this should name the slot the way the PDF does and be ready to be wrong
/// about it — the binding itself will still take.
pub fn set_gesture(slot: GestureSlot, action: GestureAction) -> Vec<u8> {
    encode_command(slot.command(), &[action.ascii()])
}

// ---------------------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------------------

/// `0x59` — push the phone's wall clock as `YY MM DD HH MM SS` in **plain hex, not BCD**.
///
/// The PDF says BCD and the PDF is wrong: `1a 07 1b 12 28 12` is 2026-07-27 18:40:18, captured.
/// A BCD encoder sets the device clock to a garbage date.
///
/// Components rather than an instant, because this crate reads no clock and no zone database —
/// see the module docs. `year` is a full year; only its last two digits go on the wire.
pub fn send_phone_time(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Vec<u8> {
    encode_command(
        AppCommand::SendPhoneTime,
        &[(year % 100) as u8, month, day, hour, minute, second],
    )
}

/// `0x48` — read the switch states. **The reply is a TEN-FRAME BURST, never a `0x48` frame.**
///
/// Feed the burst to [`SwitchStates`](crate::parser::SwitchStates); waiting for an aggregate
/// `0x48` reply waits forever, which is what the reference Android implementation does today.
pub fn get_switch_states() -> Vec<u8> {
    encode_command(AppCommand::GetSwitchStates, &[])
}

/// `0x45` — read the ten action-sync flags.
pub fn get_device_status() -> Vec<u8> {
    encode_command(AppCommand::GetDeviceStatus, &[])
}

/// `0x17` — battery. The reply's two digits are ASCII and overflow `'9'` to `0x3A` at 100 %.
pub fn get_battery() -> Vec<u8> {
    encode_command(AppCommand::GetBattery, &[])
}

/// `0x55` — bt / isp firmware versions and the hardware revision.
pub fn get_versions() -> Vec<u8> {
    encode_command(AppCommand::GetVersions, &[])
}

/// `0x64` — project and customer codes (`"T1"` / `"0303"` on every captured unit).
pub fn get_project_name() -> Vec<u8> {
    encode_command(AppCommand::GetProjectName, &[])
}

/// `0x95` — the 4-byte capability word. Also arrives UNSOLICITED ~30 ms after every link-up.
pub fn get_capabilities() -> Vec<u8> {
    encode_command(AppCommand::GetCapabilities, &[])
}

/// `0x14` — factory reset. CLIENT-ONLY: declared by both reference implementations and the PDF, never observed.
pub fn factory_reset() -> Vec<u8> {
    encode_command(AppCommand::FactoryReset, &[])
}

/// `0x60` — reboot. CLIENT-ONLY.
pub fn reboot() -> Vec<u8> {
    encode_command(AppCommand::Reboot, &[])
}

/// `0x63` — enter upgrade mode; `p2p` selects `0x31` over `0x30`, as with the Wi-Fi opens.
pub fn enter_upgrade(p2p: bool) -> Vec<u8> {
    let byte = if p2p { 0x31 } else { 0x30 };
    encode_command(AppCommand::EnterUpgrade, &[byte])
}

/// `0x65` — report the just-flashed ISP version after a successful OTA. CLIENT-ONLY.
pub fn send_isp_version(major: u8, minor: u8, patch: u8) -> Vec<u8> {
    encode_command(AppCommand::SendIspVersion, &[major, minor, patch])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::decode_app;

    /// The captured outbound frame, byte for byte. capture `client_1` writes all three LED
    /// levels; this is the middle one.
    #[test]
    fn set_led_matches_the_captured_frame() {
        assert_eq!(
            set_led(LedLevel::Mid),
            vec![0xAB, 0x55, 0x00, 0x03, 0x01, 0x31, 0x32]
        );
    }

    /// The two boolean conventions are NOT the same byte, and this is the test that says so.
    /// A settings write that used the media encoding would send `0x01`, which the firmware
    /// accepts and reads as neither on nor off.
    #[test]
    fn settings_booleans_are_ascii_and_media_booleans_are_raw() {
        assert_eq!(
            decode_app(&set_voice_command(true)).unwrap().data,
            vec![0x31]
        );
        assert_eq!(
            decode_app(&set_wear_detection(false)).unwrap().data,
            vec![0x30]
        );
        assert_eq!(decode_app(&play_pause(true)).unwrap().data, vec![0x01]);
        assert_eq!(
            decode_app(&voice_recording(false)).unwrap().data,
            vec![0x00]
        );
    }

    #[test]
    fn record_duration_is_two_byte_big_endian() {
        // 60 s and 180 s, both captured.
        assert_eq!(
            decode_app(&set_record_duration(60)).unwrap().data,
            vec![0x00, 0x3C]
        );
        assert_eq!(
            decode_app(&set_record_duration(180)).unwrap().data,
            vec![0x00, 0xB4]
        );
    }

    /// Plain hex, not BCD. A BCD encoder would put `0x26` here for year 26 and set the clock
    /// to a garbage date.
    #[test]
    fn phone_time_is_plain_hex_not_bcd() {
        let f = decode_app(&send_phone_time(2026, 7, 27, 18, 40, 18)).unwrap();
        assert_eq!(f.data, vec![0x1A, 0x07, 0x1B, 0x12, 0x28, 0x12]);
    }

    /// Both Wi-Fi modes are one function, so a client cannot implement half of the pair
    /// without noticing — which is exactly what Android did with `0x67`.
    #[test]
    fn both_wifi_services_open_through_one_builder() {
        assert_eq!(
            decode_app(&open_wifi(WifiService::Files, false))
                .unwrap()
                .cmd,
            0x39
        );
        assert_eq!(
            decode_app(&open_wifi(WifiService::Live, false))
                .unwrap()
                .cmd,
            0x67
        );
        assert_eq!(
            decode_app(&open_wifi(WifiService::Live, true))
                .unwrap()
                .data,
            vec![0x31]
        );
    }

    /// The three `0x44` forms differ only in two payload bytes and mean entirely different
    /// things to the device; pinned together so one cannot be edited into another.
    #[test]
    fn the_three_download_completions_stay_distinct() {
        assert_eq!(
            decode_app(&file_download_complete()).unwrap().data,
            vec![0x30, 0x00]
        );
        assert_eq!(decode_app(&power_off_isp()).unwrap().data, vec![0x30, 0x01]);
        assert_eq!(
            decode_app(&file_download_partial(7)).unwrap().data,
            vec![0x31, 0x07]
        );
    }

    /// Every no-payload read carries the protocol's `0x00` filler (§2.1.1), which the captured
    /// reads all do.
    #[test]
    fn no_payload_reads_carry_the_mandated_filler() {
        for frame in [
            get_battery(),
            get_versions(),
            get_project_name(),
            get_switch_states(),
        ] {
            assert_eq!(decode_app(&frame).unwrap().data, vec![0x00]);
        }
        assert_eq!(
            get_battery(),
            vec![0xAB, 0x55, 0x00, 0x03, 0x17, 0x00, 0x17]
        );
    }

    /// A gesture setter and its slot share a byte, and the value is the action's ASCII digit —
    /// the captured defaults round-trip through the setter.
    #[test]
    fn gesture_setters_round_trip_the_captured_defaults() {
        for (slot, want) in GestureSlot::ALL
            .into_iter()
            .zip(GestureSlot::CAPTURED_DEFAULTS)
        {
            let action = GestureAction::from_value(want).expect("a captured default maps");
            let f = decode_app(&set_gesture(slot, action)).unwrap();
            assert_eq!(f.cmd, slot.code());
            assert_eq!(f.data, vec![want]);
        }
    }

    #[test]
    fn volume_addresses_one_channel_at_a_time_and_carries_the_level_verbatim() {
        // The captured slider writes: `70 01 07`, `70 00 06`, `70 02 06`.
        assert_eq!(
            decode_app(&set_volume(VolumeChannel::Media, 0x07))
                .unwrap()
                .data,
            vec![0x01, 0x07]
        );
        assert_eq!(
            decode_app(&set_volume(VolumeChannel::System, 0x06))
                .unwrap()
                .data,
            vec![0x00, 0x06]
        );
        assert_eq!(
            decode_app(&set_volume(VolumeChannel::Call, 0x06))
                .unwrap()
                .data,
            vec![0x02, 0x06]
        );
    }

    #[test]
    fn led_levels_round_trip_the_burst_encoding() {
        for level in LedLevel::ALL {
            assert_eq!(LedLevel::from_level(level.ascii() - b'0'), Some(level));
        }
        assert_eq!(LedLevel::from_level(3), None, "no clamp");
    }

    /// Every builder produces a frame that decodes — checksum, length and all — so a
    /// hand-edited payload cannot ship a frame nothing can read back.
    #[test]
    fn every_builder_emits_a_decodable_frame() {
        let frames = [
            take_photo(true),
            start_video(),
            stop_video(),
            voice_recording(true),
            get_file_count(),
            open_wifi(WifiService::Files, false),
            file_download_complete(),
            file_download_partial(3),
            power_off_isp(),
            switch_music(true),
            play_pause(false),
            volume_step(true),
            answer_hangup(false),
            set_volume(VolumeChannel::Call, 8),
            get_volumes(),
            interrupt_voice(),
            retransmit_voice(),
            get_voice_disable_state(),
            set_led(LedLevel::High),
            set_record_duration(90),
            set_wear_detection(true),
            set_voice_command(false),
            set_orientation(Orientation::Landscape),
            set_offline_voice_language(true),
            set_gesture(GestureSlot::DoubleTap, GestureAction::NextTrack),
            send_phone_time(2026, 1, 1, 0, 0, 0),
            get_switch_states(),
            get_device_status(),
            get_battery(),
            get_versions(),
            get_project_name(),
            get_capabilities(),
            factory_reset(),
            reboot(),
            enter_upgrade(true),
            send_isp_version(1, 3, 1),
        ];
        for f in &frames {
            let decoded = decode_app(f).expect("builder output decodes");
            assert!(
                AppCommand::from_code(decoded.cmd).is_some(),
                "builder emitted an opcode outside the table: {:#04x}",
                decoded.cmd
            );
            assert!(!decoded.data.is_empty(), "§2.1.1 forbids an empty payload");
        }
    }
}
