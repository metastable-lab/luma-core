//! The I/O half — sockets, a radio, and a codec. Every bit of it behind a feature flag.
//!
//! The rest of this crate is sans-IO and stays that way: builders and parsers, no dependency, no
//! socket, no clock. That is the right shape for a library three platforms embed. It is the
//! wrong shape for a person with a laptop and two hours who wants a photo out of a pair of
//! glasses, and this module is the bridge.
//!
//! | feature | module | dependencies | what it does |
//! |---|---|---|---|
//! | `wifi-client` | [`fileapi`] | `ureq` | list / thumbnail / download / delete over the SoftAP (§13) |
//! | `ble` | [`ble`] | `btleplug`, `tokio`, `futures` | a real BLE central: connect, handshake, photo, voice, Wi-Fi (§1–§12) |
//! | `opus` | [`opus`] | `opus` (system libopus) | wake-word packets → 16 kHz PCM, and a WAV writer (§10) |
//!
//! None is on by default and none is required by any other. `cargo tree` on the default build
//! still prints one line.
//!
//! Everything here is a THIN driver over the sans-IO modules: the URLs come from
//! [`crate::fileapi`], the frames from [`crate::commands`], the decode from
//! [`crate::parser`], and the durations from [`crate::timing`]. Nothing in here re-derives a
//! protocol fact, which is the property that keeps the two halves from drifting.

#[cfg(feature = "ble")]
pub mod ble;

#[cfg(feature = "wifi-client")]
pub mod fileapi;

#[cfg(feature = "opus")]
pub mod opus;
