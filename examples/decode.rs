//! Decode a device → app byte stream into typed events.
//!
//! ```text
//! cargo run --example decode
//! cargo run --example decode -- ac550005173a300182
//! ```
//!
//! With no argument it replays a connect blob stitched from real captures: the unsolicited
//! `0x95` capability push, the `0x55` version reply, the `0x64` project name, the `0x17`
//! battery read and the whole ten-frame `0x48` switch-state burst. The blob is deliberately
//! fed to the parser in RAGGED chunks, because that is what a notification stream looks like.

use luma_core::parser::{Parser, SwitchStates};

/// Real frames, concatenated. Every one of these is a golden vector in `src/frame.rs`.
const CONNECT_BLOB: &str = concat!(
    "ac550006950000040ba4",       // capability push, ~30 ms after link-up
    "ac550009550104080103010269", // versions: bt V1.4.8 / isp V1.3.1 / hw V2
    "ac55000a645431000030333033af", // project "T1", customer "0303"
    "ac550005173a300182",         // battery 100 %, charging (ASCII, '9'+1 = 0x3A)
    "ac55000c450000000000000000000045", // action sync — TEN bytes
    // the 0x48 switch-state burst: ten frames, and not a 0x48 among them
    "ac550003013132",
    "ac55000402003c3e",
    "ac550003043135",
    "ac550003063137",
    "ac550003073037",
    "ac550003083139",
    "ac55000309323b",
    "ac550003103444",
    "ac550003113344",
    "ac550003613091",
    "ac55000f2544482d5477492d34363431453467", // SoftAP SSID
);

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(s.len() % 2 == 0, "hex string must have an even length");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex"))
        .collect()
}

fn main() {
    let arg = std::env::args().nth(1);
    let hex = arg.as_deref().unwrap_or(CONNECT_BLOB);
    let bytes = unhex(hex);

    println!("feeding {} bytes\n", bytes.len());

    let mut parser = Parser::new();
    let mut states = SwitchStates::new();

    // Ragged on purpose: the deframer must survive a frame split across two notifications and
    // two frames coalesced into one.
    for chunk in bytes.chunks(7) {
        for event in parser.push(chunk) {
            states.ingest(&event);
            println!("{event:?}");
        }
    }

    println!("\nsettings after the burst:");
    println!("  led               {:?}", states.led());
    println!("  record seconds    {:?}", states.record_seconds());
    println!("  wear detection    {:?}", states.wear_detection());
    println!("  voice command     {:?}", states.voice_command());
    println!("  orientation       {:?}", states.orientation());
    for slot in luma_core::GestureSlot::ALL {
        println!("  gesture {slot:<18?} {:?}", states.gesture(slot));
    }
    println!("  complete          {}", states.is_complete());

    if !parser.buffered().is_empty() {
        println!("\n{} bytes left buffered (a partial frame)", parser.buffered().len());
    }
}
