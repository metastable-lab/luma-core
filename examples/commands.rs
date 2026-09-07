//! Print every app → device frame this crate can build, as hex.
//!
//! ```text
//! cargo run --example commands
//! ```
//!
//! A wire cheat-sheet, and a smoke test: every line is a complete
//! `AB 55 | len(2 BE) | cmd | payload | checksum` frame you can write straight to `AA13`.
//! The opcode and its evidence level are read back OUT of the built frame rather than typed
//! beside it, so this listing cannot drift from the builders.
//!
//! `Capture` means the byte was seen on the wire; `DeviceProbe` means it was answered by a
//! unit under test; `ClientOnly` means a vendor app declares it and no capture contains it.

use luma_core::commands as cmd;
use luma_core::opcodes::{AppCommand, GestureAction, GestureSlot, VolumeChannel};
use luma_core::parser::{Orientation, WifiService};
use luma_core::{decode_app, LedLevel};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Decode our own output so the opcode column is derived, never asserted.
fn show(name: &str, frame: Vec<u8>) {
    let decoded = decode_app(&frame).expect("every builder emits a decodable frame");
    let op = AppCommand::from_code(decoded.cmd);
    let evidence = op.map(|o| format!("{:?}", o.evidence())).unwrap_or_else(|| "?".into());
    let named = op.map(|o| format!("{o:?}")).unwrap_or_else(|| "?".into());
    println!(
        "{:<36} {:#04x} {:<22} {:<11} {}",
        name,
        decoded.cmd,
        named,
        evidence,
        hex(&frame)
    );
}

fn main() {
    println!(
        "{:<36} {:<4} {:<22} {:<11} {}",
        "builder", "op", "AppCommand", "evidence", "frame"
    );
    println!("{}", "-".repeat(118));

    println!("\n-- capture ------------------------------------------------------------------");
    show("take_photo(false)", cmd::take_photo(false));
    show("take_photo(true)   # for AI", cmd::take_photo(true));
    show("start_video()", cmd::start_video());
    show("stop_video()", cmd::stop_video());
    show("voice_recording(true)", cmd::voice_recording(true));
    show("voice_recording(false)", cmd::voice_recording(false));

    println!("\n-- files and Wi-Fi ----------------------------------------------------------");
    show("get_file_count()", cmd::get_file_count());
    show("open_wifi(Files, false)", cmd::open_wifi(WifiService::Files, false));
    show("open_wifi(Live, false)", cmd::open_wifi(WifiService::Live, false));
    show("open_wifi(Files, true)  # p2p", cmd::open_wifi(WifiService::Files, true));
    show("file_download_complete()", cmd::file_download_complete());
    show("file_download_partial(3)", cmd::file_download_partial(3));
    show("power_off_isp()", cmd::power_off_isp());

    println!("\n-- media and call -----------------------------------------------------------");
    show("switch_music(true)", cmd::switch_music(true));
    show("play_pause(true)", cmd::play_pause(true));
    show("volume_step(true)", cmd::volume_step(true));
    show("answer_hangup(true)", cmd::answer_hangup(true));
    show("set_volume(System, 7)", cmd::set_volume(VolumeChannel::System, 7));
    show("set_volume(Media, 7)", cmd::set_volume(VolumeChannel::Media, 7));
    show("set_volume(Call, 7)", cmd::set_volume(VolumeChannel::Call, 7));
    show("get_volumes()", cmd::get_volumes());

    println!("\n-- voice --------------------------------------------------------------------");
    show("interrupt_voice()  # closes the mic", cmd::interrupt_voice());
    show("retransmit_voice()", cmd::retransmit_voice());
    show("get_voice_disable_state()", cmd::get_voice_disable_state());

    println!("\n-- settings -----------------------------------------------------------------");
    show("set_led(Low)", cmd::set_led(LedLevel::Low));
    show("set_led(High)", cmd::set_led(LedLevel::High));
    show("set_record_duration(180)", cmd::set_record_duration(180));
    show("set_wear_detection(true)", cmd::set_wear_detection(true));
    show("set_voice_command(true)", cmd::set_voice_command(true));
    show("set_orientation(Portrait)", cmd::set_orientation(Orientation::Portrait));
    show("set_orientation(Landscape)", cmd::set_orientation(Orientation::Landscape));
    show("set_offline_voice_language(true)", cmd::set_offline_voice_language(true));

    println!("\n-- gesture slots ------------------------------------------------------------");
    for slot in GestureSlot::ALL {
        show(
            &format!("set_gesture({slot:?}, PlayPause)"),
            cmd::set_gesture(slot, GestureAction::PlayPause),
        );
    }

    println!("\n-- device -------------------------------------------------------------------");
    show(
        "send_phone_time(2026-07-27 18:40:18)",
        cmd::send_phone_time(2026, 7, 27, 18, 40, 18),
    );
    show("get_switch_states()  # burst reply", cmd::get_switch_states());
    show("get_device_status()", cmd::get_device_status());
    show("get_battery()", cmd::get_battery());
    show("get_versions()", cmd::get_versions());
    show("get_project_name()", cmd::get_project_name());
    show("get_capabilities()", cmd::get_capabilities());
    show("factory_reset()", cmd::factory_reset());
    show("reboot()", cmd::reboot());
    show("enter_upgrade(false)", cmd::enter_upgrade(false));
    show("send_isp_version(1, 3, 1)", cmd::send_isp_version(1, 3, 1));

    println!("\nvendor-named bytes that are NOT commands. Recognising one reads as permission");
    println!("to send it, so `AppCommand::from_code` returns None for every one:");
    for (code, name) in luma_core::VENDOR_NAMED_UNIMPLEMENTED {
        println!("  {code:#04x}  {name}");
    }
}
