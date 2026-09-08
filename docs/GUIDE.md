# Building with Luma Core

The integration guide. It walks every flow the glasses support, from connecting to
watching the live camera, with the exact library calls in Rust, Swift and Python.
[`PROTOCOL.md`](../PROTOCOL.md) has the bytes behind each step; the section numbers
below (§n) point into it.

- [1. The model](#1-the-model)
- [2. Setup](#2-setup)
- [3. Connect and handshake](#3-connect-and-handshake)
- [4. Battery, status and versions](#4-battery-status-and-versions)
- [5. Take a photo, get the AI image over BLE](#5-take-a-photo-get-the-ai-image-over-ble)
- [6. Voice: from wake word to speech-to-text](#6-voice-from-wake-word-to-speech-to-text)
- [7. Gallery over Wi-Fi: list, download, delete](#7-gallery-over-wi-fi-list-download-delete)
- [8. Live view](#8-live-view)
- [9. Settings and gestures](#9-settings-and-gestures)
- [10. Media and calls](#10-media-and-calls)
- [11. Timing you own](#11-timing-you-own)
- [12. Event reference](#12-event-reference)
- [13. The laptop path](#13-the-laptop-path)
- [14. Troubleshooting](#14-troubleshooting)

## 1. The model

Luma Core is three layers, and you choose how deep to go.

| layer | what it is | dependencies |
|---|---|---|
| **sans-IO core** (default build) | builders and parsers for every byte on BLE, the file API and the RTSP/RTP stream. Pure functions and small state machines. | none |
| **clients** (feature flags) | a blocking HTTP client for the gallery (`wifi-client`), an async BLE client (`ble`), Opus-to-WAV (`opus`) | `ureq`, `btleplug` + `tokio`, `opus` |
| **your app** | the radio, the Wi-Fi join, the screen | whatever you like |

On a phone you use the core through the Swift or Kotlin bindings and write the radio
yourself (the iOS demo app is a copy-and-paste starting point). On a laptop or a Pi you can
use the clients and never touch a socket.

The rule that makes the core portable: **it reads no clock and opens no socket**. Anything
that needs a timer, the core names and you implement. §11 lists every duration with a
value that works, and the `timing` module carries them as constants.

## 2. Setup

**Rust**

```toml
[dependencies]
luma-core = { git = "https://github.com/metastable-lab/luma-core" }
# optional:
# luma-core = { git = "…", features = ["ble", "wifi-client", "opus"] }
```

**Python** (any platform with a BLE adapter)

```bash
./bindings/generate.sh python
cp target/release/libluma_core.dylib bindings/out/python/     # .so on Linux
pip install bleak
export PYTHONPATH=bindings/out/python
```

**Swift** — run `./ios/build-core.sh` once, then `import LumaCore` (see the iOS section of
the README).

**Kotlin** — `./bindings/generate.sh kotlin`, build the crate for your ABIs with
`--features uniffi`, add `net.java.dev.jna:jna@aar`.

The generated APIs mirror the Rust one with a `glasses` prefix: `commands::get_battery()`
is `glassesGetBattery()` in Swift and `glasses_get_battery()` in Python; `Parser` is
`GlassesParser`; `DeviceEvent` is `FfiGlassesEvent`.

## 3. Connect and handshake

The sequence (§3): subscribe to both notify characteristics, expect a capabilities push,
read versions, project name, battery, status, all switch states, volumes, capabilities
and voice-feature state, then send the phone's time.

**Rust, with the `ble` feature** — the client does the sequence for you:

```rust
use luma_core::client::ble::Glasses;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let glasses = Glasses::connect_first(Duration::from_secs(10)).await?;
    let hs = glasses.handshake().await?;              // versions, identity, battery, switch states
    println!("{:?} battery {:?}", hs.versions, hs.battery);
    glasses.send_time(2026, 9, 8, 12, 0, 0).await?;   // §16: plain hex, the client encodes it
    Ok(())
}
```

**Swift** — you own CoreBluetooth; the library owns every byte:

```swift
import CoreBluetooth
import LumaCore

let gatt = glassesGatt()          // .service "AA12", .write "AA13", .notifyAll ["AA14","AA15"]
let parser = GlassesParser()      // one per link; feed it AA14 verbatim
let files = GlassesFileReassembler()   // feed it AA15 verbatim

// after discovering characteristics and calling setNotifyValue(true) on both notify chars:
let connectGroup: [[UInt8]] = [
    glassesGetVersions(), glassesGetProjectName(), glassesGetBattery(),
    glassesGetDeviceStatus(), glassesGetSwitchStates(), glassesGetVolumes(),
    glassesGetCapabilities(), glassesGetVoiceDisableState(),
]
for frame in connectGroup {
    peripheral.writeValue(Data(frame), for: writeChar, type: .withResponse)
}

func peripheral(_ p: CBPeripheral, didUpdateValueFor c: CBCharacteristic, error: Error?) {
    guard let data = c.value else { return }
    if c.uuid.uuidString == gatt.controlNotify {
        for event in parser.push(chunk: [UInt8](data)) { handle(event) }
    } else if c.uuid.uuidString == gatt.fileNotify {
        for event in files.push(chunk: [UInt8](data)) { handleFile(event) }
    }
}
```

Every write is a **Write Request** (`.withResponse`, `WRITE_TYPE_DEFAULT` on Android).
`gatt.writeWithResponse` says so at runtime if you want to assert it.

**Python** (`python/demo.py` is the full version):

```python
import asyncio, luma_core as luma
from bleak import BleakClient, BleakScanner

gatt = luma.glasses_gatt()
sig = lambda short: f"0000{short.lower()}-0000-1000-8000-00805f9b34fb"

async def main():
    dev = await BleakScanner.find_device_by_filter(
        lambda d, ad: sig(gatt.service) in [u.lower() for u in ad.service_uuids])
    parser = luma.GlassesParser()
    async with BleakClient(dev) as c:
        for u in gatt.notify_all:
            await c.start_notify(sig(u), lambda _, data: [print(e) for e in parser.push(bytes(data))])
        for frame in (luma.glasses_get_versions(), luma.glasses_get_battery(),
                      luma.glasses_get_switch_states(), luma.glasses_get_capabilities()):
            await c.write_gatt_char(sig(gatt.write), frame, response=True)
        await asyncio.sleep(5)

asyncio.run(main())
```

**What comes back.** `Versions`, `Identity`, `Battery`, `ActionSync`, then ten `Switch` /
`GestureBinding` events (the `0x48` burst, §7), `Volumes`, `Capabilities`,
`VoiceFeatures`. Use `SwitchStates` (`GlassesSwitchStates`) to accumulate the burst; it
reports `is_complete()` after the tenth frame.

## 4. Battery, status and versions

- **Battery** arrives two ways: as a reply to `get_battery()` and unsolicited as a push
  every ~10 s (60 s while charging). Both decode to `DeviceEvent::Battery { percent,
  charging, source }`. Polling every 5–10 s is plenty; the push does the rest.
- **Status** (`get_device_status()` → `ActionSync`) tells you what the glasses are doing
  right now: taking a photo, recording, a Wi-Fi session open (`importingMode`), and the
  touchpad gestures that just fired (`volumeUp`, `volumeDown`, `playPause`). Poll every
  ~30 s, or right after an action to confirm it took.
- **Versions** and **Identity** are static; read them once.

```rust
match event {
    DeviceEvent::Battery(b) => println!("{}% {}", b.percent, if b.charging { "charging" } else { "" }),
    DeviceEvent::ActionSync(s) => for a in s.active() { println!("active: {a:?}") },
    DeviceEvent::Versions(v) => println!("bt {:?} isp {:?} hw {}", v.bt, v.isp, v.hardware),
    _ => {}
}
```

## 5. Take a photo, get the AI image over BLE

Two photo commands (§9):

| | saves a full photo? | pushes a small JPEG over BLE? |
|---|---|---|
| `take_photo(false)` | yes, to EVENT | no |
| `take_photo(true)` | yes, to EVENT | yes, ~11 KB, about a second after the ack (§15) |

The small image arrives on `AA15` in the `52 58` file framing. Feed those notifications to
a `FileReassembler`; it yields `Started { declared_total, file_type }`, then
`Completed(ReassembledFile)` with the JPEG bytes, or `Aborted` / `Desynced` if the transfer
breaks. Budget about 3.2 s from write to bytes (`timing::PHOTO_CAPTURE_TYPICAL`).

```rust
// Rust, `ble` feature: the client waits for the transfer
let jpeg = glasses.take_photo(true).await?;      // Option<Vec<u8>>
std::fs::write("look.jpg", jpeg.expect("no image"))?;
```

```swift
// Swift: you already feed AA15 into `files` (section 3)
peripheral.writeValue(Data(glassesTakePhoto(forAi: true)), for: writeChar, type: .withResponse)
func handleFile(_ e: FfiGlassesFileEvent) {
    if case .completed(_, _, let data) = e { showImage(UIImage(data: Data(data))) }
}
```

That image is what to send to a vision model for "what am I looking at". Full-resolution
photos only come off over Wi-Fi (section 7).

## 6. Voice: from wake word to speech-to-text

When the wake word fires, the glasses stream Opus packets on `AA14` (§10). The parser
turns them into `DeviceEvent::Voice(VoiceEvent)`: `Started`, then `Packet(VoicePacket)`
about 50 times a second, then `Ended(cause)` when **you** close it.

**The stream never ends by itself.** You run two timers and write `interrupt_voice()`
(`0x56`) when either fires; the library then emits `Ended`. Use `Parser::close_voice(cause)`
to close the state machine at the same moment.

| timer | constant | value |
|---|---|---|
| no packet for this long → the user stopped talking | `timing::VOICE_IDLE_GAP` | 1.2 s |
| absolute cap on one capture | `timing::VOICE_HARD_CAP` | 30 s |

```rust
// Rust, `ble` + `opus`: capture, decode, save
use luma_core::client::opus::{VoiceDecoder, write_wav};
let packets = glasses.voice_capture_default().await?;          // runs both timers, writes 0x56
let mut dec = VoiceDecoder::new()?;                              // 16 kHz mono
let pcm = dec.decode_all(&packets)?;
write_wav(std::path::Path::new("voice.wav"), &pcm)?;             // ready for any STT API
```

Each packet is one 20 ms SILK wideband frame, mono; ask your decoder for **16 kHz**
output. Without the `opus` feature, `VoicePacket.payload` goes straight into a platform
decoder:

| platform | decoder |
|---|---|
| iOS | `AVAudioConverter` from an Opus `AVAudioFormat`, or libopus via SwiftPM |
| Android | `MediaCodec` (`audio/opus`) |
| Python | `opuslib` / `pyogg` |

`VoicePacket` also carries the parsed header (`OpusToc`): frame duration, bandwidth,
`decoded_samples(rate)` so you can size buffers before decoding.

A voice assistant loop is then: wait for `Started` → collect packets → on idle gap, write
`0x56` → decode → speech-to-text → your model → text-to-speech on the phone. Enable the
wake word with `set_voice_command(true)` if capabilities report it off.

## 7. Gallery over Wi-Fi: list, download, delete

The glasses raise their own access point and serve a small HTTP API on it (§12–§13).
The flow is the same on every platform:

1. Over BLE, write `open_wifi(WifiService::Files, false)` (`0x39`).
2. Wait for `DeviceEvent::WifiCredentials { ssid }` (about 2.5 s). The passphrase is fixed:
   `WifiCredentials::PASSPHRASE` / `glassesWifiPassphrase()`.
3. Wait `timing::SSID_SETTLE` (2 s), then join the network. The glasses are
   `fileapi::HOST` (`192.168.169.1`).
4. List, download, delete over HTTP.
5. Over BLE, write `file_download_complete()` (`0x44 30 00`) and leave the network.

**Rust, `wifi-client` feature** (the laptop must already be on the AP; `examples/gallery.rs`
prints the join command when it is not):

```rust
use luma_core::client::fileapi::FileClient;
let c = FileClient::new();
let list = c.list()?;
for f in list.all_files() {
    println!("{:<8} {:>6} KiB  {}", f.folder.name(), f.size_kib, f.name);
}
let bytes = c.download(list.all_files().next().unwrap())?;   // verified against size_kib
c.delete("EVENT/20260727225147720.jpg")?;
let report = c.sync_all(std::path::Path::new("gallery"), true)?;   // download everything, then delete
```

**Any platform, using only the core** — the URLs, the parsing and the completeness rule
come from the library; you bring the HTTP client:

```swift
// Swift
let list = try glassesWifiParseFileList(json: String(decoding: data, as: UTF8.self))
for folder in list.folders { for f in folder.files {
    let url = URL(string: glassesWifiDownloadUrl(name: f.name))!
    let thumb = URL(string: glassesWifiThumbnailUrl(name: f.name))!
    // after downloading:
    let ok = glassesWifiDownloadIsComplete(sizeKib: f.sizeKib, bytes: UInt64(bytes.count))
}}
let reply = try glassesWifiParseDeleteReply(json: deleteBody)   // .success
```

```python
# Python (python/gallery.py is the full CLI)
import urllib.request, luma_core as luma
body = urllib.request.urlopen(luma.glasses_wifi_list_url()).read().decode()
listing = luma.glasses_wifi_parse_file_list(body)
for folder in listing.folders:
    for f in folder.files:
        data = urllib.request.urlopen(luma.glasses_wifi_download_url(f.name)).read()
        assert luma.glasses_wifi_download_is_complete(f.size_kib, len(data))
```

Folders: **EVENT** photos, **AAC** voice recordings, **LOOP** video clips, **EMR**
(emergency, always empty so far). The listing's `size` is in **KiB**, so a download is
complete when `size_kib * 1024 <= bytes < (size_kib + 1) * 1024`; that is what
`download_is_complete` checks, and why `FileClient::download` refuses a short body.
`SyncPlan` gives the recommended order (list → thumbnails → per file download then delete)
if you are writing your own loop.

**Joining the network.** iOS: `NEHotspotConfiguration(ssid:passphrase:isWEP:)` with
`joinOnce = true`, plus the Hotspot Configuration entitlement; remove the configuration
when done so the phone snaps back. Android: `WifiNetworkSpecifier` +
`ConnectivityManager.requestNetwork`, and bind your HTTP calls to that network. macOS:
`networksetup -setairportnetwork en0 <SSID> 12345678`. Linux: `nmcli dev wifi connect
<SSID> password 12345678`. Expect the join to need a retry sometimes; budget
`timing::WIFI_JOIN_TIMEOUT` (25 s).

## 8. Live view

Same network dance with `open_wifi(WifiService::Live, false)` (`0x67`), then RTSP (§14).
The library carries the whole protocol side:

| step | library |
|---|---|
| RTSP conversation (OPTIONS → DESCRIBE → SETUP → PLAY, TEARDOWN) | `rtsp::RtspSession`: `next_request()` gives bytes to send over TCP, `feed(bytes)` parses what comes back |
| the SDP | `sdp::parse`, or `session.sdp()` after DESCRIBE |
| RTP headers, loss and reordering | `rtp::parse`, `rtp::SequenceTracker` |
| H.264 → Annex-B access units | `h264::Depacketizer::push(packet)`; `parameter_sets_annex_b()`; `AccessUnit::is_decodable_start()` |
| AAC audio (optional) | `aac::Depacketizer::push(packet)` → raw frames; `aac::adts_frame` to write a playable `.aac` |

```rust
// std only — examples/live.rs is the complete version
use luma_core::{h264, rtp, rtsp::RtspSession};
let mut s = RtspSession::new();                       // rtsp://192.168.169.1:554/h264, video ports 8712/8713
while let Some(req) = s.next_request() { tcp.write_all(&req)?; /* read reply into buf */ s.feed(&buf)?; }
let mut video = h264::Depacketizer::new();
loop {
    let n = udp.recv(&mut pkt)?;
    for au in video.push(&pkt[..n]) {
        if au.is_decodable_start() || started { file.write_all(&au.data)?; started = true; }
    }
}
```

Decode with a hardware decoder: `VideoToolbox` / `AVSampleBufferDisplayLayer` on iOS
(convert the Annex-B start codes to 4-byte lengths and build the format description from
the SPS/PPS the depacketizer hands you), `MediaCodec` on Android, `ffplay live.h264` on a
laptop. Start on the first keyframe that came with its parameter sets; starting elsewhere
shows grey until the next IDR. The stream is 1600×1200 at ~25 fps and ~3 Mbps.

Audio is advertised (AAC-LC 16 kHz mono) and works when requested; make it best-effort
so a refused audio `SETUP` never blocks video.

Stop: `session.teardown()` over TCP, then `file_download_complete()` over BLE, then leave
the network.

## 9. Settings and gestures

| setting | write | read back as |
|---|---|---|
| wear detection | `set_wear_detection(bool)` | `Switch(WearDetection)` |
| wake word / voice command | `set_voice_command(bool)` | `Switch(VoiceCommand)` |
| clip length | `set_record_duration(secs)` | `Switch(RecordSeconds)` |
| camera orientation | `set_orientation(Orientation)` | `Switch(Orientation)` |
| indicator LED brightness | `set_led(LedLevel)` | `Switch(Led)` (status light only, §18) |
| a gesture binding | `set_gesture(GestureSlot, GestureAction)` | `GestureBinding { slot, action }` |

Each setter is echoed back as its acknowledgement, and `get_switch_states()` returns all of
them as a burst of ten frames (§7). `SwitchStates` accumulates the burst and exposes typed
getters (`wear_detection()`, `gesture(slot)`, …).

Five gesture slots — swipe forward, swipe back, single, double and triple tap — each bound
to one of the media actions (volume up/down, play/pause, previous/next). To use gestures
for your own purposes, watch `ActionSync` for `volumeUp`/`volumeDown`/`playPause` firing,
or rebind slots so each gesture maps to a distinct action you can tell apart.

## 10. Media and calls

`switch_music(next)`, `play_pause(play)`, `answer_hangup(answer)` are what the glasses
send toward the phone's media session when the user taps. `set_volume(channel, level)`
sets one of three channels (system, media, call); `get_volumes()` reads all three. Use
`set_volume` for playback volume, not `volume_step`, which only moves the system channel
(§11).

## 11. Timing you own

All of these are in `timing` (Rust) and as `glasses_timing_*_ms()` (bindings). None are
measured by the library; each is a number that works on this firmware.

| constant | value | used for |
|---|---|---|
| `SSID_ARRIVAL_TYPICAL` | 2.5 s | how long after `0x39`/`0x67` the SSID arrives |
| `SSID_SETTLE` | 2 s | wait after the SSID before joining |
| `WIFI_JOIN_TIMEOUT` | 25 s | give up on a join |
| `VOICE_IDLE_GAP` | 1.2 s | end a capture when packets stop |
| `VOICE_HARD_CAP` | 30 s | end a capture regardless |
| `VOICE_INTERRUPT_REPEAT_GAP` | 0.5 s | a second `0x56`, for safety |
| `BATTERY_POLL_MIN` / `MAX` | 5 s / 10 s | how often to ask, if you ask at all |
| `STATE_REFRESH` | 30 s | re-read status, switches, volumes, capabilities |
| `CAPABILITIES_PUSH_DELAY` | 30 ms | the unsolicited `0x95` after link-up |
| `FILE_STREAM_STALL` | 5 s | abandon a `52 58` transfer with no new frames |
| `PHOTO_CAPTURE_TYPICAL` | 3.2 s | write → AI image complete |

## 12. Event reference

Everything `Parser::push` can return (`FfiGlassesEvent` in the bindings):

| event | from | meaning |
|---|---|---|
| `Battery(BatteryReading)` | `0x17` reply, `0x53` push | percent, charging, which source |
| `Versions(Versions)` | `0x55` | `bt`, `isp`, `hardware` |
| `Identity(Identity)` | `0x64` | project and customer codes |
| `MediaCount(MediaCount)` | `0x42` | files waiting on the glasses |
| `Switch(SwitchReport)` | setter echoes, `0x48` burst | one setting's value |
| `GestureBinding { slot, action, raw }` | `0x07`–`0x11` | one gesture slot's binding |
| `Capabilities(Capabilities)` | `0x95` | the raw capability word |
| `VoiceFeatures(VoiceFeatures)` | `0x71` | which voice features are disabled |
| `Volumes(Volumes)` | `0x69` | system, media, call |
| `WifiCredentials(WifiCredentials)` | `0x25` | the SSID; `passphrase()` is the fixed key |
| `WifiOpening { serves, raw }` | `0x39`/`0x67` ack | the AP is coming up, for files or live |
| `ActionSync(ActionSync)` | `0x45` | the ten status flags |
| `Voice(VoiceEvent)` | `0x97`/`0x46` | `Started`, `Packet`, `Ended(cause)` |
| `VoiceFrame { cmd, data }` | | a voice-family frame outside a capture |
| `VoiceAbandoned` / `AiBroadcastCancelled` / `HdImageFailed` / `IspUpgradeFinished` | `0x49`/`0x51`/`0x52`/`0x96` | defined, rarely seen |
| `Ack { command, data }` | any setter echo | the glasses accepted a command |
| `Malformed { cmd, data, reason }` | | a known command with a payload that did not parse |
| `Unrecognised { cmd, data }` | | a frame with an unknown command byte |

The `52 58` file stream (`AA15`) has its own events from `FileReassembler`: `Started`,
`Completed(ReassembledFile)`, `Aborted(FileAbort)`, `Desynced { dropped }`.

## 13. The laptop path

No phone required. With the `ble` feature the crate is a complete client:

```bash
cargo run --example luma --features ble -- scan
cargo run --example luma --features ble -- info                 # handshake, print everything
cargo run --example luma --features ble -- photo --ai look.jpg  # the BLE image
cargo run --example luma --features ble,opus -- voice out.wav   # wake word → WAV
cargo run --example luma --features ble -- wifi gallery         # raise the AP, print the SSID
cargo run --example gallery --features wifi-client -- sync ./gallery   # once joined
cargo run --example luma --features ble -- wifi live
cargo run --example live -- 30 ./out                            # → live.h264 + live.aac
cargo run --example luma --features ble -- wifi close
```

macOS asks for Bluetooth permission for the terminal the first time; a denied permission
looks exactly like "no glasses found". `python/demo.py` and `python/gallery.py` are the
same path in Python.

## 14. Troubleshooting

| symptom | likely cause |
|---|---|
| writes succeed, nothing ever comes back | not subscribed to `AA14`, or writing without response |
| waiting for a `0x48` reply | there is none; collect the ten per-setting frames (§7) |
| voice capture never ends | it never does on its own; write `0x56` on your timers (§6) |
| battery shows 99 forever | decoding `0x17` as two ASCII digits; at 100 % the tens digit is `0x3A`. The library handles it |
| record duration reads as garbage | it is a 2-byte integer inside a burst of ASCII digits (§7) |
| glasses' clock is a strange date | `0x59` sent as BCD; it is plain hex (§16) |
| volume buttons do nothing for music | `0x32` moves the system channel only; use `set_volume(Media, …)` |
| every download "truncated" | comparing bytes to `size`; `size` is KiB (§13) |
| Wi-Fi join fails right after the SSID | join too early; wait `SSID_SETTLE` (§12) |
| join keeps failing on iOS | expected sometimes; retry, and suggest turning off cellular data |
| live video grey then fine | started before a keyframe; wait for `is_decodable_start()` |
| iOS demo: no devices found | the Simulator has no radio; run on a physical iPhone |
| `ld: library 'luma_core' not found` in Xcode | run `./ios/build-core.sh` first |
