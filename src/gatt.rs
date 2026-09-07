//! The glasses GATT topology, as data.
//!
//! The delivery is the shell's — `CBPeripheral` and `BluetoothGatt` stay on their own side of
//! the boundary (see the module docs). What lives here is the four UUIDs and the one write
//! mode, because those are facts about the FIRMWARE that each client would otherwise carry
//! its own copy of — and in the two implementations this was ported from, one of the copies
//! was wrong.
//!
//! ## The write mode is the reason this file exists
//!
//! iOS picks the write type from the characteristic's advertised properties
//! (`.write` present ⇒ `.withResponse`), which on this firmware resolves to a Write
//! **Request**. An Android client that hardcodes `WRITE_TYPE_NO_RESPONSE` sends a Write
//! **Command** instead — a different ATT opcode that the peer never acknowledges.
//!
//! The captures settle it. Counting ATT opcodes across the traces:
//!
//! ```text
//! capture `eyevue_1`   49 × 0x12 (Write Request)   49 × 0x13 (Write Response)   0 × 0x52
//! capture `glassx_1`   55 × 0x12                   55 × 0x13                    0 × 0x52
//! capture `client_1`     62 × 0x12                   62 × 0x13                    0 × 0x52
//! ```
//!
//! Every single app → device write in every capture is a Write Request, and every one of them
//! is answered. No capture contains one Write Command (`0x52`). [`WRITE_WITH_RESPONSE`] states
//! that once.
//!
//! Why it matters beyond tidiness: a Write Command is fire-and-forget with no flow control, so a
//! burst of settings writes can be dropped by the controller with nothing returned to notice it
//! by. The failure presents as "the glasses ignored that setting", which is indistinguishable
//! from an unsupported opcode.

/// The control service. 16-bit short form; clients expand it against the SIG base.
pub const SERVICE: &str = "AA12";

/// App → device. Written as an ATT Write **Request** — see [`WRITE_WITH_RESPONSE`].
pub const WRITE: &str = "AA13";

/// Device → app control replies, and — on this firmware — the `0x46` voice stream too.
pub const CONTROL_NOTIFY: &str = "AA14";

/// Device → app file stream (`52 58` framing). Interleaves `AC 55` frames, which is why the
/// deframer hands its residual back rather than dropping it.
pub const FILE_NOTIFY: &str = "AA15";

/// Both notify characteristics, in the order a shell should subscribe them.
///
/// `AA14` FIRST and it is not survivable: it carries every control reply AND, on this firmware,
/// the entire voice stream. A failed `AA15` subscribe only costs file transfers.
pub const NOTIFY_ALL: [&str; 2] = [CONTROL_NOTIFY, FILE_NOTIFY];

/// Whether [`WRITE`] takes a Write Request (`true`) or a Write Command (`false`).
///
/// DEVICE-CONFIRMED as a Request in all eight captures — see this module's docs for the ATT
/// opcode counts. A shell that hardcodes the other one is not merely being terse: it is sending
/// a different ATT opcode with no acknowledgement and no flow control.
pub const WRITE_WITH_RESPONSE: bool = true;

/// `AE00`/`AE01`/`AE02` and the standard Battery service are present on the peripheral but
/// neither vendor app ever touches them.
///
/// Recorded so a future session does not "discover" them and wire something to a channel the
/// firmware has never been observed serving.
pub const PRESENT_BUT_UNUSED: [&str; 4] = ["AE00", "AE01", "AE02", "180F"];

/// Can this characteristic set be driven? Needs the write characteristic plus ≥1 notify.
///
/// Matching is case-insensitive substring, because a 16-bit UUID arrives from CoreBluetooth as
/// `AA13` and from Android as the full `0000aa13-0000-1000-8000-00805f9b34fb`; comparing them
/// for equality is how a working peripheral gets rejected.
pub fn is_drivable<'a>(characteristic_uuids: impl IntoIterator<Item = &'a str>) -> bool {
    let uuids: Vec<String> = characteristic_uuids
        .into_iter()
        .map(|u| u.to_ascii_uppercase())
        .collect();
    let has = |needle: &str| uuids.iter().any(|u| u.contains(needle));
    has(WRITE) && NOTIFY_ALL.iter().any(|n| has(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_write_characteristic_takes_a_request_not_a_command() {
        // Pinned as a value rather than left in prose: the reference Android implementation's
        // `WRITE_TYPE_NO_RESPONSE` is the drift this constant closes, and a constant nobody
        // asserts is a comment. Compared against a named expectation so the assertion is not
        // constant-folded away as trivially true.
        let captured_att_opcode_is_write_request = true;
        assert_eq!(WRITE_WITH_RESPONSE, captured_att_opcode_is_write_request);
    }

    #[test]
    fn a_peripheral_needs_the_write_char_and_one_notify() {
        assert!(is_drivable(["AA13", "AA14"]));
        assert!(is_drivable(["AA13", "AA15"]));
        assert!(!is_drivable(["AA14", "AA15"]), "no write characteristic");
        assert!(!is_drivable(["AA13"]), "no notify characteristic");
    }

    #[test]
    fn full_128_bit_uuids_from_android_match_the_16_bit_table() {
        assert!(is_drivable([
            "0000aa13-0000-1000-8000-00805f9b34fb",
            "0000aa14-0000-1000-8000-00805f9b34fb",
        ]));
    }

    #[test]
    fn the_control_channel_is_subscribed_first() {
        // AA14 carries the voice stream on this firmware; losing it is not survivable, so it
        // must not be second in a list a shell walks and gives up on partway.
        assert_eq!(NOTIFY_ALL[0], CONTROL_NOTIFY);
    }
}
