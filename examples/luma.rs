//! Drive the glasses from a laptop, over BLE. Feature `ble` (and `opus` for `.wav`).
//!
//! ```text
//! cargo run --example luma --features ble -- scan
//! cargo run --example luma --features ble -- info
//! cargo run --example luma --features ble -- photo [--ai out.jpg]
//! cargo run --example luma --features ble,opus -- voice [out.wav]
//! cargo run --example luma --features ble -- voice out.opus-packets
//! cargo run --example luma --features ble -- wifi gallery|live|close
//! cargo run --example luma --features ble -- settings
//! ```
//!
//! This is the whole hackathon path with no phone in it: scan for service `AA12`, connect,
//! handshake, take a photo, capture the microphone, raise the Wi-Fi. Every frame is built by
//! [`luma_core::commands`] and every reply decoded by [`luma_core::parser`]; this file owns the
//! CLI and nothing else.
//!
//! **macOS:** the first run pops a Bluetooth permission prompt for the terminal. If it does not,
//! grant it under System Settings → Privacy & Security → Bluetooth and run again — a denied
//! permission looks exactly like "no glasses found".
//!
//! Nothing here has been run against hardware from the environment it was written in: no
//! adapter, no unit. The protocol underneath is pinned by the crate's golden vectors.

use std::time::Duration;

use luma_core::client::ble::{BleError, Glasses, GlassesEvent};
use luma_core::parser::WifiService;
use luma_core::{fileapi, timing};

const USAGE: &str = "\
usage: luma <command>

  scan                      list peripherals advertising service AA12
  info                      connect, run the §3 handshake, print everything read
  photo [--ai <file>]       take a photo; --ai also pulls the ~11 KB image over BLE
  voice [<file>]            capture one wake-word utterance
                              *.wav             decoded PCM   (needs --features opus)
                              anything else     raw Opus packets, one per line
  wifi gallery              raise the SoftAP for the file API (0x39) and print the SSID
  wifi live                 raise it for the RTSP live stream (0x67)
  wifi close                tear it down (0x44 30 00)
  settings                  print the ten-frame switch-state burst
  listen [seconds]          connect and print every event, for poking at the device
";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{USAGE}");
        std::process::exit(2);
    }
    if let Err(e) = run(&args).await {
        eprintln!("\n{e}");
        if matches!(e, BleError::NotFound) {
            eprintln!(
                "\nNothing is advertising {}. Wake the glasses, make sure no phone is holding\n\
                 the connection, and on macOS check that this terminal has Bluetooth permission\n\
                 (System Settings → Privacy & Security → Bluetooth).",
                luma_core::gatt::SERVICE
            );
        }
        std::process::exit(1);
    }
}

const SCAN: Duration = Duration::from_secs(8);

async fn run(args: &[String]) -> Result<(), BleError> {
    match args[0].as_str() {
        "scan" => cmd_scan().await,
        "info" => cmd_info().await,
        "photo" => cmd_photo(&args[1..]).await,
        "voice" => cmd_voice(&args[1..]).await,
        "wifi" => cmd_wifi(&args[1..]).await,
        "settings" => cmd_settings().await,
        "listen" => cmd_listen(&args[1..]).await,
        other => {
            eprintln!("unknown command `{other}`\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn cmd_scan() -> Result<(), BleError> {
    println!(
        "scanning {SCAN:?} for service {}…",
        luma_core::gatt::SERVICE
    );
    let found = Glasses::scan(SCAN).await?;
    if found.is_empty() {
        return Err(BleError::NotFound);
    }
    for d in &found {
        println!(
            "  {}  {}  rssi {}",
            d.id,
            d.name.as_deref().unwrap_or("(unnamed)"),
            d.rssi.map(|r| r.to_string()).unwrap_or_else(|| "?".into())
        );
    }
    println!(
        "\n{} found. `luma info` connects to the strongest.",
        found.len()
    );
    Ok(())
}

async fn connect() -> Result<Glasses, BleError> {
    println!("scanning…");
    let g = Glasses::connect_first(SCAN).await?;
    println!("connected to {}", g.id());
    Ok(g)
}

async fn cmd_info() -> Result<(), BleError> {
    let g = connect().await?;
    let h = g.handshake().await?;
    println!("\nversions        {:?}", h.versions);
    println!("identity        {:?}", h.identity);
    println!("battery         {:?}", h.battery);
    println!("capabilities    {:?}", h.capabilities);
    println!("voice features  {:?}", h.voice_features);
    println!("volumes         {:?}", h.volumes);
    print_switches(&h.switches);
    println!("\nhandshake complete: {}", h.is_complete());
    g.disconnect().await
}

fn print_switches(s: &luma_core::parser::SwitchStates) {
    println!("\nsettings (the 0x48 burst — ten frames, and not a 0x48 among them):");
    println!("  led             {:?}", s.led());
    println!("  record seconds  {:?}", s.record_seconds());
    println!("  wear detection  {:?}", s.wear_detection());
    println!("  voice command   {:?}", s.voice_command());
    println!("  orientation     {:?}", s.orientation());
    for slot in luma_core::GestureSlot::ALL {
        println!("  gesture {slot:<18?} {:?}", s.gesture(slot));
    }
    println!("  complete        {}", s.is_complete());
}

async fn cmd_settings() -> Result<(), BleError> {
    let g = connect().await?;
    let h = g.handshake().await?;
    print_switches(&h.switches);
    g.disconnect().await
}

async fn cmd_photo(args: &[String]) -> Result<(), BleError> {
    let ai = args.first().map(|a| a == "--ai").unwrap_or(false);
    let out = args.get(1).cloned().unwrap_or_else(|| "ai.jpg".into());

    let g = connect().await?;
    println!(
        "taking a photo{}… (capture takes ~{:?})",
        if ai { " with the AI image" } else { "" },
        timing::PHOTO_CAPTURE_TYPICAL
    );
    match g.take_photo(ai).await? {
        Some(jpeg) => {
            std::fs::write(&out, &jpeg).map_err(|e| BleError::Gatt {
                operation: format!("write {out}"),
                detail: e.to_string(),
            })?;
            println!("{} bytes → {out}", jpeg.len());
            println!("The FULL-resolution photo is in the EVENT folder and only comes off");
            println!("over Wi-Fi: `luma wifi gallery`, then the gallery example.");
        }
        None => {
            println!("done. The file is in the EVENT folder — pull it over Wi-Fi:");
            println!("  cargo run --example luma --features ble -- wifi gallery");
            println!("  cargo run --example gallery --features wifi-client -- sync ./gallery");
        }
    }
    g.disconnect().await
}

async fn cmd_voice(args: &[String]) -> Result<(), BleError> {
    let out = args.first().cloned();
    let g = connect().await?;

    println!(
        "listening. Say the wake word.\n  \
         stops after {:?} of silence, or {:?} whatever happens.\n  \
         0x56 is written either way — it is the only thing that closes the microphone.",
        timing::VOICE_IDLE_GAP,
        timing::VOICE_HARD_CAP
    );
    let packets = g.voice_capture_default().await?;
    println!(
        "\n{} Opus packets ({:.1}s at 50/s), {} bytes",
        packets.len(),
        packets.len() as f64 / 50.0,
        packets.iter().map(|p| p.payload.len()).sum::<usize>()
    );
    if let Some(toc) = packets.first().and_then(|p| p.toc) {
        println!("  first packet: {:?} {:?}", toc.mode(), toc.bandwidth());
    }

    if let Some(path) = out {
        write_voice(&path, &packets)?;
    }
    g.disconnect().await
}

#[cfg(feature = "opus")]
fn write_voice(path: &str, packets: &[luma_core::VoicePacket]) -> Result<(), BleError> {
    use luma_core::client::opus::{write_packet_dump, write_wav, VoiceDecoder};
    if path.ends_with(".wav") {
        let mut d = VoiceDecoder::new().map_err(|e| BleError::Gatt {
            operation: "opus".into(),
            detail: e.to_string(),
        })?;
        let (pcm, skipped) = d.decode_all_counted(packets);
        write_wav(std::path::Path::new(path), &pcm).map_err(|e| BleError::Gatt {
            operation: format!("write {path}"),
            detail: e.to_string(),
        })?;
        println!(
            "  {} samples ({:.1}s at 16 kHz) → {path}{}",
            pcm.len(),
            pcm.len() as f64 / 16_000.0,
            if skipped > 0 {
                format!(", {skipped} packet(s) libopus refused")
            } else {
                String::new()
            }
        );
    } else {
        write_packet_dump(std::path::Path::new(path), packets).map_err(|e| BleError::Gatt {
            operation: format!("write {path}"),
            detail: e.to_string(),
        })?;
        println!("  raw Opus packets → {path}");
    }
    Ok(())
}

#[cfg(not(feature = "opus"))]
fn write_voice(path: &str, packets: &[luma_core::VoicePacket]) -> Result<(), BleError> {
    use std::io::Write;
    if path.ends_with(".wav") {
        println!("  .wav needs the `opus` feature: cargo run --example luma --features ble,opus");
        println!("  writing the raw packets instead.");
    }
    let mut f = std::fs::File::create(path).map_err(|e| BleError::Gatt {
        operation: format!("create {path}"),
        detail: e.to_string(),
    })?;
    for p in packets {
        let hex: String = p.payload.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(f, "{} {}", p.payload.len(), hex).ok();
    }
    println!("  raw Opus packets → {path}");
    Ok(())
}

async fn cmd_wifi(args: &[String]) -> Result<(), BleError> {
    let which = args.first().map(String::as_str).unwrap_or("gallery");
    let g = connect().await?;

    if which == "close" {
        g.close_wifi().await?;
        println!("0x44 30 00 written. Leave the network on this machine too.");
        return g.disconnect().await;
    }

    let service = match which {
        "live" => WifiService::Live,
        _ => WifiService::Files,
    };
    println!("raising the access point ({which})…");
    let ssid = g.open_wifi(service).await?;

    println!("\n  SSID        {ssid}");
    println!("  passphrase  {}", fileapi::PASSPHRASE);
    println!(
        "  glasses at  {}   (you get {})",
        fileapi::HOST,
        fileapi::PHONE_ADDRESS
    );
    println!("\njoin it:");
    println!(
        "  macOS:  networksetup -setairportnetwork en0 {ssid} {}",
        fileapi::PASSPHRASE
    );
    println!(
        "  Linux:  nmcli dev wifi connect {ssid} password {}",
        fileapi::PASSPHRASE
    );
    println!("\nthen:");
    if service == WifiService::Files {
        println!("  cargo run --example gallery --features wifi-client -- list");
    } else {
        println!("  cargo run --example live -- 10");
    }
    println!("\nand when you are done:  luma wifi close");

    // The BLE link stays up while the AP is: `0x44` has to be written on it afterwards.
    println!("\n(leaving the BLE link connected for 60 s so you can join the AP…)");
    tokio::time::sleep(Duration::from_secs(60)).await;
    g.disconnect().await
}

async fn cmd_listen(args: &[String]) -> Result<(), BleError> {
    let seconds: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30);
    let g = connect().await?;
    let mut rx = g.events();
    println!("printing every event for {seconds}s. Touch the pad, take a photo, talk.");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while tokio::time::Instant::now() < deadline {
        let left = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(GlassesEvent::Disconnected)) => break,
            Ok(Ok(e)) => println!("  {e:?}"),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                println!("  … {n} events missed (the voice stream outran this loop)")
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    // Never leave the microphone open.
    g.interrupt_voice().await.ok();
    g.disconnect().await
}
