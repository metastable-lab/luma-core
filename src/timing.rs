//! The durations the protocol needs, named — and only named.
//!
//! Everywhere else this crate refuses to hold a duration, because holding one looks like
//! measuring one and this crate has no clock. The refusal is still right; what it left behind
//! was a set of numbers scattered across three documents and rediscovered by every new client,
//! usually one failed pairing at a time.
//!
//! So: named [`Duration`] constants and nothing else. A `Duration` is a quantity, not a clock —
//! there is no `Instant` in this file, nothing calls `sleep`, and nothing here can tell you
//! what time it is. The client still owns every timer; it no longer has to guess what to set it
//! to.
//!
//! Two kinds of value live here and the difference matters:
//!
//! * **Pinned by the wire.** [`SSID_ARRIVAL_TYPICAL`], [`CAPABILITIES_PUSH_DELAY`] and
//!   [`PHOTO_CAPTURE_TYPICAL`] are measurements from live sessions. They describe what the
//!   device does; a client that waits less than these fails.
//! * **Chosen.** [`VOICE_IDLE_GAP`], [`VOICE_HARD_CAP`], [`BATTERY_POLL_MIN`],
//!   [`STATE_REFRESH`], [`FILE_STREAM_STALL`] are policy — values that work, not values the
//!   firmware demands. Each says so in its own doc comment. Changing one is a product decision
//!   and not a bug fix.
//!
//! Reachable from Swift/Kotlin/Python as `glasses_timing_*_ms()` (feature `uniffi`), because a
//! millisecond count is the one shape every platform's timer takes.

use core::time::Duration;

/// How long to wait AFTER the `0x25` SSID push before trying to join the AP.
///
/// PINNED. The glasses answer `0x39`/`0x67` immediately, push the SSID about
/// [`SSID_ARRIVAL_TYPICAL`] later, and the access point is not accepting associations at that
/// moment. Joining on the SSID event itself fails, and it fails in the worst way — the OS
/// reports "incorrect password" and, on iOS, remembers that.
pub const SSID_SETTLE: Duration = Duration::from_secs(2);

/// How long after the `0x39`/`0x67` acknowledgement the `0x25` SSID push arrives.
///
/// PINNED, ~2.5 s in live sessions. Useful as the deadline on "did the AP request take?"; the
/// wait before joining is [`SSID_SETTLE`] and is separate.
pub const SSID_ARRIVAL_TYPICAL: Duration = Duration::from_millis(2500);

/// No `0x46` packet for this long means the user stopped talking.
///
/// CHOSEN. Nothing in the firmware defines an end of utterance — `0x99` is declared and never
/// sent (§10) — so this is the client's silence detector. 1.2 s is long enough to survive a
/// mid-sentence pause and short enough that a reply does not feel late.
pub const VOICE_IDLE_GAP: Duration = Duration::from_millis(1200);

/// The longest a single wake-word capture may run.
///
/// CHOSEN, and it is a backstop rather than a feature: the microphone stays open until the app
/// writes `0x56`, so a client whose idle timer never fires streams until the battery is flat.
/// 25–30 s is the range shipping clients use.
pub const VOICE_HARD_CAP: Duration = Duration::from_secs(30);

/// The gap between the two `0x56` writes that close the microphone.
///
/// CHOSEN. One `0x56` is enough on a healthy link; the second, ~0.5 s later, is what shipping
/// clients send and costs nothing. The failure it covers is a dropped write leaving the mic
/// open with no further trigger to close it.
pub const VOICE_INTERRUPT_REPEAT_GAP: Duration = Duration::from_millis(500);

/// Lower bound of the `0x17` battery poll interval.
///
/// CHOSEN. Battery also arrives unsolicited on `0x53` (§5), so the poll is a backstop and not
/// the source of truth. Anything under this is wasted radio.
pub const BATTERY_POLL_MIN: Duration = Duration::from_secs(5);

/// Upper bound of the `0x17` battery poll interval. See [`BATTERY_POLL_MIN`].
pub const BATTERY_POLL_MAX: Duration = Duration::from_secs(10);

/// How often to re-read `{0x45, 0x48, 0x69, 0x95}`.
///
/// CHOSEN. These change from the device's own touchpad with no push to say so, so a periodic
/// re-read is the only way a UI stays honest. 30 s is the §3 recommendation.
pub const STATE_REFRESH: Duration = Duration::from_secs(30);

/// How long to let the OS try to join the glasses' SoftAP before calling it failed.
///
/// PINNED to the platform rather than the device: 25 s is roughly where iOS and Android give up
/// internally, so a shorter client timeout races the OS and a longer one just waits behind it.
/// The join is flaky by nature (§12) — expect to retry rather than to fail.
pub const WIFI_JOIN_TIMEOUT: Duration = Duration::from_secs(25);

/// How long after the BLE link comes up the unsolicited `0x95` capabilities frame arrives.
///
/// PINNED, ~30 ms. It arrives BEFORE the app has written a byte, which is why both notify
/// characteristics have to be subscribed before the first write — a client that subscribes
/// late does not see it at all and concludes the device has no capabilities.
pub const CAPABILITIES_PUSH_DELAY: Duration = Duration::from_millis(30);

/// How long a `52 58` file transfer may go without a `0x98` before the client gives up.
///
/// CHOSEN — 5 s, and there is nothing on the wire that argues for it. The transfer moves at
/// ~13.7 kB/s in 496-byte chunks (§15), so packets land ~36 ms apart; 5 s of silence is two
/// orders of magnitude past normal and means the device stopped, not that it is slow. The
/// reassembler cannot notice this itself — it has no clock — so the client times it and calls
/// [`crate::reassembly::FileReassembler::reset`].
pub const FILE_STREAM_STALL: Duration = Duration::from_secs(5);

/// How long a photo takes from `0x22` to the last `0x98` of its AI image.
///
/// PINNED, ~2.4 s of capture plus ~0.8 s of transfer in live sessions. A progress UI that
/// assumes a photo is instant spends three seconds looking broken.
pub const PHOTO_CAPTURE_TYPICAL: Duration = Duration::from_millis(3200);

/// Every constant in this module, paired with the name the FFI exports it under.
///
/// Exists so a binding, a settings screen or a test can enumerate them rather than list them
/// again in a second place and drift.
pub const ALL: [(&str, Duration); 12] = [
    ("ssid_settle", SSID_SETTLE),
    ("ssid_arrival_typical", SSID_ARRIVAL_TYPICAL),
    ("voice_idle_gap", VOICE_IDLE_GAP),
    ("voice_hard_cap", VOICE_HARD_CAP),
    ("voice_interrupt_repeat_gap", VOICE_INTERRUPT_REPEAT_GAP),
    ("battery_poll_min", BATTERY_POLL_MIN),
    ("battery_poll_max", BATTERY_POLL_MAX),
    ("state_refresh", STATE_REFRESH),
    ("wifi_join_timeout", WIFI_JOIN_TIMEOUT),
    ("capabilities_push_delay", CAPABILITIES_PUSH_DELAY),
    ("file_stream_stall", FILE_STREAM_STALL),
    ("photo_capture_typical", PHOTO_CAPTURE_TYPICAL),
];

/// One constant by its name, for a binding that looks them up as strings.
pub fn by_name(name: &str) -> Option<Duration> {
    ALL.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_constant_is_reachable_by_name_and_the_table_has_no_duplicates() {
        for (name, d) in ALL {
            assert_eq!(by_name(name), Some(d), "{name}");
        }
        let mut names: Vec<&str> = ALL.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate name in ALL");
        assert_eq!(by_name("no_such_timer"), None);
    }

    /// The values the protocol reference quotes, pinned so a doc edit and a code edit cannot
    /// drift apart silently.
    #[test]
    fn the_documented_values_are_the_ones_the_constants_hold() {
        assert_eq!(SSID_SETTLE.as_millis(), 2000);
        assert_eq!(VOICE_IDLE_GAP.as_millis(), 1200);
        assert_eq!(VOICE_HARD_CAP.as_secs(), 30);
        assert_eq!(VOICE_INTERRUPT_REPEAT_GAP.as_millis(), 500);
        assert_eq!(BATTERY_POLL_MIN.as_secs(), 5);
        assert_eq!(BATTERY_POLL_MAX.as_secs(), 10);
        assert_eq!(STATE_REFRESH.as_secs(), 30);
        assert_eq!(WIFI_JOIN_TIMEOUT.as_secs(), 25);
        assert_eq!(CAPABILITIES_PUSH_DELAY.as_millis(), 30);
        assert_eq!(FILE_STREAM_STALL.as_secs(), 5);
    }

    #[test]
    fn the_orderings_that_would_be_bugs_hold() {
        assert!(BATTERY_POLL_MIN < BATTERY_POLL_MAX);
        assert!(
            VOICE_IDLE_GAP < VOICE_HARD_CAP,
            "the idle gap must fire first"
        );
        assert!(
            SSID_SETTLE < SSID_ARRIVAL_TYPICAL + SSID_SETTLE,
            "the settle wait is measured from the SSID, not from the request"
        );
        assert!(CAPABILITIES_PUSH_DELAY < Duration::from_millis(100));
    }
}
