# Luma Core

**The Rust SDK and protocol reference for Luma smart glasses.**

A small, dependency-free library that speaks the glasses' Bluetooth LE protocol: it turns
the bytes the glasses send into typed events, and turns the actions you want into frames
you can write. Bindings for **Swift, Kotlin and Python** are generated from the same
source, and a one-screen **iOS demo app** shows a live connection end to end.

If you are here for a hackathon, start with the [Quickstart](#quickstart), skim
[What you can build](#what-you-can-build), then keep [`docs/GUIDE.md`](docs/GUIDE.md)
open while you code and [`PROTOCOL.md`](PROTOCOL.md) beside it for the byte-level detail.

```text
your app            Luma Core                 the glasses
─────────           ─────────                 ───────────
"take a photo" ──▶  commands::take_photo() ──▶  AB 55 00 03 22 30 52   (write to AA13)
                                              ◀── AC 55 00 05 17 3A 30 01 82  (notify on AA14)
Battery { 100 %, charging } ◀── Parser::push()
```

## What's in the box

| | |
|---|---|
| `src/` | The library: opcodes, frame codec, streaming parser, file-transfer reassembly, voice-stream state machine, GATT table, the Wi-Fi file API, the RTSP/RTP/H.264/AAC live path, recommended timings. **Zero dependencies** by default. |
| `src/client/` | Optional clients behind feature flags: an async BLE client (`ble`), a blocking gallery client (`wifi-client`), Opus-to-WAV (`opus`). With these the crate is a complete laptop client. |
| `PROTOCOL.md` | The complete wire-protocol reference: every command, every reply, every quirk, with a verification status on each. |
| `docs/GUIDE.md` | The integration guide: connect, handshake, photos, voice, Wi-Fi gallery, live video, settings, timing rules, troubleshooting. |
| `examples/` | `decode` and `commands` (offline), `gallery` (list / download / delete over Wi-Fi), `live` (record the live stream to `.h264` + `.aac`), `luma` (a full BLE CLI: scan, info, photo, voice, wifi). |
| `bindings/` | One script that generates the Swift, Kotlin and Python bindings. |
| `ios/` | `LumaDemo`, a SwiftUI app that scans, connects and drives the glasses through the generated Swift bindings. |
| `python/` | `demo.py` (BLE, on [bleak](https://github.com/hbldh/bleak)) and `gallery.py` (the Wi-Fi gallery with `urllib`). |

## The glasses

Luma is a pair of camera glasses built on the **E09** platform (project `T1`). This
library and its documentation were verified against firmware **bt 1.4.8 / isp 1.3.1 /
hw 2**.

| Capability | How you reach it |
|---|---|
| Camera: photos and video clips | BLE commands; full-resolution files come off over Wi-Fi |
| A small "AI" still (~11 KB JPEG) | Pushed over BLE right after a photo, for vision models |
| Microphone with on-device wake word | Opus packets streamed over BLE, ~50 per second |
| Touchpad gestures (tap, double, triple, swipe) | Bindable to media actions; state reported over BLE |
| Speaker, media and call control | BLE commands, three volume channels |
| Live camera view | Wi-Fi SoftAP + RTSP, H.264 1600×1200 at ~25 fps |
| Gallery (photos, voice notes, video) | Wi-Fi SoftAP + a small JSON HTTP API |
| Battery, firmware versions, wear detection | BLE reads and unsolicited pushes |

Bluetooth LE is the control plane. Wi-Fi comes up on demand, only for moving media.

## How it works, in sixty seconds

Luma Core is a **sans-IO** library. It has no Bluetooth stack, opens no socket and reads no
clock. You own the radio (CoreBluetooth, `BluetoothGatt`, bleak, btleplug, anything);
the library owns the protocol.

That split is what makes it portable: the same tested Rust runs inside an iPhone app, an
Android app, a Python script on a laptop, or a Raspberry Pi. Your side is three calls:

1. **Write** the bytes a command builder returns to characteristic `AA13`, with response.
2. **Feed** every notification from `AA14` into a `Parser` and act on the events it returns.
3. **Feed** every notification from `AA15` into a `FileReassembler` when you expect a file.

The same split holds for Wi-Fi: the file API's URLs and parsing, and the RTSP, RTP,
H.264 and AAC layers of the live stream, are sans-IO modules; a blocking HTTP client and a
socket-driven live recorder sit beside them behind feature flags. Every duration the
protocol needs (how long to wait for the network, when to stop a voice capture) is a
named constant in `timing`; the library never measures one itself.

## Quickstart

### Rust

```toml
[dependencies]
luma-core = { git = "https://github.com/metastable-lab/luma-core" }
```

```rust
use luma_core::commands;
use luma_core::parser::{DeviceEvent, Parser};

fn main() {
    // 1. Build a frame and write it to AA13 (with response).
    let frame = commands::get_battery();
    assert_eq!(frame, [0xAB, 0x55, 0x00, 0x03, 0x17, 0x00, 0x17]);

    // 2. Feed every AA14 notification to the parser, exactly as it arrives.
    //    The parser owns the deframer, so split or coalesced frames both work.
    let mut parser = Parser::new();
    let reply = [0xACu8, 0x55, 0x00, 0x05, 0x17, 0x3A, 0x30, 0x01, 0x82];
    for event in parser.push(&reply) {
        if let DeviceEvent::Battery(b) = event {
            println!("battery {}% charging={}", b.percent, b.charging); // 100% true
        }
    }
}
```

```bash
cargo test                                  # 194 tests, no dependencies
cargo run --example decode                  # a real connect sequence → typed events
cargo run --example decode -- ac550005173a300182
cargo run --example commands                # every command frame, as hex

# with hardware — the whole protocol from a laptop, no phone:
cargo run --example luma --features ble -- info                   # scan, connect, handshake
cargo run --example luma --features ble -- photo --ai look.jpg    # the ~11 KB image over BLE
cargo run --example luma --features ble,opus -- voice out.wav     # wake-word audio → WAV
cargo run --example luma --features ble -- wifi gallery           # raise the AP, print the SSID
cargo run --example gallery --features wifi-client -- sync ./out  # list, download, delete
cargo run --example live -- 30 ./out                              # → live.h264, live.aac
```

### Python

```bash
./bindings/generate.sh python
cp target/release/libluma_core.dylib bindings/out/python/     # .so on Linux
pip install bleak
PYTHONPATH=bindings/out/python python3 python/demo.py         # scans, connects, prints events
PYTHONPATH=bindings/out/python python3 python/demo.py --wifi gallery   # raise the AP
PYTHONPATH=bindings/out/python python3 python/gallery.py list          # once joined
```

```python
import luma_core as luma

gatt = luma.glasses_gatt()                  # service AA12, write AA13, notify AA14 + AA15
frame = luma.glasses_get_versions()         # bytes to write to AA13
parser = luma.GlassesParser()
for event in parser.push(notification):     # bytes from AA14
    print(event)                            # e.g. FfiGlassesEvent.BATTERY(percent=100, charging=True)
```

### iOS (Swift)

```bash
./ios/build-core.sh          # cross-compiles the crate + generates the Swift bindings (required once)
open ios/LumaDemo.xcodeproj  # run LumaDemo on a physical iPhone
```

```swift
import LumaCore

let gatt = glassesGatt()                     // UUIDs come from the library, never hardcoded
peripheral.writeValue(Data(glassesGetBattery()), for: writeChar, type: .withResponse)

let parser = GlassesParser()
func didUpdateValue(_ data: Data) {          // AA14 notification
    for event in parser.push(chunk: [UInt8](data)) {
        if case .battery(let percent, let charging, _) = event { /* … */ }
    }
}
```

See [iOS demo app](#ios-demo-app) below for the gallery and live-view screens. The whole
CoreBluetooth side of the demo is one file, `ios/LumaDemo/GlassesLink.swift`, and is meant
to be copied.

### Android (Kotlin)

```bash
./bindings/generate.sh kotlin     # → bindings/out/kotlin/luma/core/luma_core.kt
```

Build the crate for your ABIs with `cargo ndk` (or `cargo build --target
aarch64-linux-android --features uniffi --release`), drop the `.so` into `jniLibs/`, add
`net.java.dev.jna:jna@aar` and the generated file to your module, then use it exactly like
the Swift above: `glassesGatt()`, `glassesGetBattery()`, `GlassesParser().push(bytes)`.
Write to `AA13` with `WRITE_TYPE_DEFAULT` (a write *with* response).

## iOS demo app

`ios/` is a SwiftUI app (`luma.core.demo`, iOS 17+) with three screens, all driven by the
generated Swift bindings. The app owns CoreBluetooth, the Wi-Fi join, `URLSession`, two
sockets and a video layer; every byte of protocol comes from the library.

| screen | what it shows |
|---|---|
| **Connect** | scan for the `AA12` service, connect, run the handshake, live event log, battery and versions, photo and interrupt-voice buttons |
| **Gallery** | raise the glasses' Wi-Fi, join it, list files by folder with thumbnails, tap to download (verified against the listing's size, then shareable), swipe to delete, tear down |
| **Live** | raise the Wi-Fi for live view, RTSP through the library's session object, H.264 into `AVSampleBufferDisplayLayer` with fps and packet-loss readouts, AAC audio best-effort |

Both Wi-Fi screens print a step-by-step checklist naming the binding or frame behind each
step, so the flow is readable on the phone.

```bash
./ios/build-core.sh          # REQUIRED FIRST: cross-compiles the crate + generates the Swift bindings
cd ios && xcodegen           # only if you edited project.yml
open ios/LumaDemo.xcodeproj  # then run on a physical iPhone
```

**`ios/build-core.sh` must run before the project will build.** It compiles the crate for
`aarch64-apple-ios` and `aarch64-apple-ios-sim`, generates the Swift bindings into the local
SPM package at `ios/LumaCore/`, and copies a static archive per SDK into `ios/lib/`. That
output is gitignored, so a fresh clone has to run it. It sets `DEVELOPER_DIR` to the full
Xcode if unset.

**The Simulator has no Bluetooth radio and no hotspot API.** The app builds and its UI
can be inspected there, but every real interaction needs a paired physical iPhone, with
the *Hotspot Configuration* capability enabled on your App ID for the two Wi-Fi screens.

```text
ios/
  project.yml                       xcodegen source of truth; the .xcodeproj is committed
  build-core.sh                     cargo → static archives + generated Swift
  LumaDemo/LumaDemoApp.swift        the @main entry
  LumaDemo/GlassesLink.swift        all the CoreBluetooth, the one owner of the link
  LumaDemo/ContentView.swift        connect screen: status header, device list, event log
  LumaDemo/GlassesWiFi.swift        one-shot NEHotspotConfiguration join, reachability poll
  LumaDemo/FileApiClient.swift      URLSession over the library's URLs and parsers
  LumaDemo/GalleryScreen.swift      the gallery flow and list
  LumaDemo/LiveStreamSession.swift  RTSP over TCP, RTP over UDP, Annex-B → CMSampleBuffer
  LumaDemo/LiveAudioPlayer.swift    AAC playback via AVAudioEngine
  LumaDemo/LiveScreen.swift         the video layer and readouts
  LumaCore/                         local SPM package holding the generated bindings
  lib/                              generated static archives (gitignored)
```

The link flags (`-lluma_core`, per-SDK `LIBRARY_SEARCH_PATHS`) live at project level in
`ios/project.yml`. It is a plain static archive per SDK plus generated Swift, no
`.xcframework` and no SPM `.binaryTarget`.

## What you can build

Some starting points, roughly in order of effort:

- **A voice assistant.** The glasses stream Opus audio the moment the wake word fires.
  Decode it, send it to a speech-to-text model, answer through the speaker. See the
  voice recipe in the guide.
- **"What am I looking at?"** Ask for a photo with the AI flag, receive an ~11 KB JPEG
  over BLE about a second later, hand it to a vision model. No Wi-Fi needed.
- **Live translation or captioning.** Same audio stream, different model, results on
  your phone screen.
- **A smarter gallery.** Pull photos and voice notes over the Wi-Fi file API, tag, search,
  summarise.
- **Gesture-driven anything.** Five touchpad gestures report their state; bind them to
  your own actions instead of media control.
- **Live-view experiments.** RTSP at 1600×1200; feed it to a detector, a streamer, a
  recorder.
- **A companion for another platform.** The Python demo runs on a laptop or a Pi; the
  crate compiles anywhere Rust does.

## Verification status

Every command in the library carries a status you can read at runtime:

| `Evidence` | Meaning |
|---|---|
| `Capture` | Verified in real sessions with the glasses. The large majority of the protocol. |
| `DeviceProbe` | Tested directly against the hardware. |
| `ClientOnly` | Defined by the platform but not yet exercised. Sendable, but expect nothing. |

```rust
use luma_core::{AppCommand, Evidence};
assert_eq!(AppCommand::GetBattery.evidence(), Evidence::Capture);
assert_eq!(AppCommand::FactoryReset.evidence(), Evidence::ClientOnly);
```

`PROTOCOL.md` marks every row the same way, and lists the behaviours that differ from
what a first reading of the command table would suggest (for example: reading all
settings returns a burst of ten frames, not one; the microphone only stops when *you* tell
it to). The test suite pins each of those with real frames, byte for byte.

## Feature flags

| feature | adds |
|---|---|
| *(none)* | the library. Zero dependencies. |
| `uniffi` | the `ffi` module: ~90 functions and 5 objects that become the Swift / Kotlin / Python API |
| `bindgen` | the bindings generator binary (`uniffi-bindgen`). Off by default so its toolchain stays out of normal builds |
| `wifi-client` | `client::fileapi::FileClient`, a blocking HTTP client for the gallery (`ureq`) |
| `ble` | `client::ble::Glasses`, an async BLE client with handshake, photo, voice capture and Wi-Fi helpers (`btleplug`, `tokio`) |
| `opus` | `client::opus::VoiceDecoder` and a WAV writer (the `opus` crate; needs libopus, `brew install opus`) |

```bash
cargo test                          # 194 tests, zero dependencies
cargo test --features uniffi        # 225 tests
cargo test --features wifi-client   # adds the HTTP client
cargo build --features ble,opus --examples
cargo build --features bindgen      # the generator; bindings/generate.sh uses it
```

## Repository layout

```text
src/
  lib.rs            crate docs and re-exports
  gatt.rs           the service and characteristic UUIDs, as data
  frame.rs          the AB 55 / AC 55 envelope, checksum, streaming deframer
  opcodes.rs        every command and reply, with its verification status
  commands.rs       one builder per command: take_photo(), set_volume(), …
  parser.rs         AA14 bytes → DeviceEvent; settings accumulator
  voice.rs          wake-word audio stream state machine, Opus packet headers
  reassembly.rs     the 52 58 file stream on AA15 (the AI image)
  fileapi.rs        the Wi-Fi file API: URLs, listing + reply parsers, the KiB rule, SyncPlan
  rtsp.rs sdp.rs    the RTSP conversation as a state machine; SDP parsing
  rtp.rs h264.rs    RTP headers and loss tracking; H.264 depacketising to Annex-B
  aac.rs            AAC-hbr depacketising and ADTS headers
  timing.rs         every recommended duration, as a constant
  client/           ble.rs, fileapi.rs, opus.rs — behind feature flags
  ffi.rs            the UniFFI surface (feature "uniffi")
examples/           decode, commands, gallery, live, luma
bindings/           uniffi.toml, generate.sh
ios/                LumaDemo app, LumaCore SPM package, build-core.sh
python/             demo.py (BLE), gallery.py (Wi-Fi)
docs/GUIDE.md       integration guide
PROTOCOL.md         wire-protocol reference
```

## Contributing

Protocol changes need a real frame: add the bytes to the relevant test module with a note
on where they came from, and keep the `Evidence` level honest. `cargo test` and
`cargo test --features uniffi` must stay green, and `cargo run --example commands` is a
quick way to eyeball an encoder change.

## Licence

MIT. See [LICENSE](LICENSE).
