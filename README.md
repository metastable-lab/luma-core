# luma-core

The BLE wire protocol of a pair of E09 smart glasses, reverse-engineered from packet
captures and written as a sans-IO Rust crate with 122 tests, plus the full spec in
[`PROTOCOL.md`](PROTOCOL.md).

The crate decides and computes; it opens no socket and holds no radio. You bring the
Bluetooth stack — CoreBluetooth, `BluetoothGatt`, bleak, `btleplug` — and this turns
notification bytes into typed events and typed commands into frames you can write.

**Zero dependencies** by default. `uniffi` is the only one there is, it is optional, and
turning it on gets you Swift, Kotlin and Python bindings from the same source.

## The hardware

An **E09** unit — project `T1`, customer `0303` (`equipmentCode T10303`), bt **V1.4.8** /
isp **V1.3.1** / hw **V2**. Camera, microphone with an on-device wake word, touchpad,
speaker, and a Wi-Fi SoftAP for pulling media off.

Two vendor apps drive this same firmware and were captured side by side: **EyeVue** 3.2.7
and **GlassX** 3.0.8, both skins on the **Watchfun** platform. They share opcodes and
differ in which ones they bother to send, which is most of how the vocabulary here was
separated from the vendor's own aspirations for it.

### GATT

| | |
|---|---|
| service | `AA12` |
| write, app → device | `AA13`, ATT Write **Request** (`0x12`, with response) |
| control notify | `AA14` |
| file / voice notify | `AA15` |

`AE00`/`AE01`/`AE02` and the standard Battery service are present, and neither vendor app
touches them. Voice PCM and control replies both arrive on `AA14`; `AA15` carries the
`52 58` file stream, which is a second, unrelated framing.

```text
app → device   AB 55 | len(2, BE) | cmd | data… | crc
device → app   AC 55 | len(2, BE) | cmd | data… | crc
len = 1 (cmd) + N (data) + 1 (crc)
crc = (cmd + Σ data) & 0xFF
```

This table is also data, in the `gatt` module — read it from there rather than typing a
UUID into your client.

## Quickstart

```rust
use luma_core::commands;
use luma_core::parser::Parser;

fn main() {
    // 1. Build a frame and write it to AA13, with response.
    let frame = commands::get_battery();
    assert_eq!(frame, [0xAB, 0x55, 0x00, 0x03, 0x17, 0x00, 0x17]);

    // 2. Feed every AA14 notification straight in. `Parser` owns the deframer, so
    //    fragmented and coalesced frames both work — hand it whatever arrives.
    let mut parser = Parser::new();
    let reply = [0xACu8, 0x55, 0x00, 0x05, 0x17, 0x3A, 0x30, 0x01, 0x82];
    for event in parser.push(&reply) {
        println!("{event:?}");
        // Battery(BatteryReading { percent: 100, charging: true, source: ReadReply })
    }
}
```

Two runnable examples, both offline:

```bash
cargo run --example decode      # a captured connect blob → typed events
cargo run --example decode -- ac550005173a300182
cargo run --example commands    # every app → device frame, as hex, with its evidence level
```

## What is confirmed, and what is not

Every opcode carries its own `Evidence` level, and a test forbids rounding one upward:

| level | means | count |
|---|---|---|
| `Capture` | seen on the wire, in at least one direction | 39 |
| `DeviceProbe` | answered by a unit under test, never seen in a vendor capture | 2 |
| `ClientOnly` | a vendor app and/or the vendor PDF declares it; no capture, no probe | 12 |

```rust
use luma_core::{AppCommand, Evidence};
assert_eq!(AppCommand::GetBattery.evidence(), Evidence::Capture);
assert_eq!(AppCommand::FactoryReset.evidence(), Evidence::ClientOnly);
```

Read `ClientOnly` as "nobody has ever seen this work". Three findings contradict what a
reader will find in the vendor PDF or in either vendor app, and each is stated where it
applies:

* **`0x48` has no `0x48` reply.** The firmware answers a switch-state read with a burst of
  ten frames, each keyed by that setting's own setter opcode. Waiting for a `0x48` frame
  waits forever.
* **`0x99` (voice-upload-end) is never sent.** Four captures totalling 14,636 voice frames
  contain zero. The mic closes when the app writes `0x56` and at no other time.
* **`0x59` phone time is plain hex, not BCD**, whatever the PDF says. A BCD encoder sets
  the device clock to a garbage date.

There are also bytes the vendor's own SDK names that no capture has ever produced;
`VENDOR_NAMED_UNIMPLEMENTED` lists them, and `AppCommand::from_code` deliberately returns
`None` for every one — recognising a byte reads as permission to send it.

## Feature flags

| feature | what it adds |
|---|---|
| *(none)* | the pure library. **Zero dependencies.** |
| `uniffi` | the `ffi` module and the `uniffi` dependency — the FFI surface, ~90 free functions and 5 objects |
| `bindgen` | `uniffi` plus the generator toolchain and the `uniffi-bindgen` binary. Off by default so `uniffi/cli`'s subtree (clap, `cargo_metadata`, and through it a newer rustc floor) stays out of every build that only wants the library |

```bash
cargo test                          # 122 tests, no dependencies
cargo test --features uniffi        # 138 tests, adds the FFI surface
cargo build --features bindgen      # the bindings generator
```

## Bindings — Swift, Kotlin, Python

```bash
./bindings/generate.sh              # all three
./bindings/generate.sh python       # or just one
```

Output lands in `bindings/out/<language>/` (gitignored). Module names come from
`bindings/uniffi.toml`: Swift module `LumaCore`, Kotlin package `luma.core`,
Python module `luma_core`.

Python, end to end:

```bash
./bindings/generate.sh python
cp target/release/libluma_core.dylib bindings/out/python/   # .so on Linux
PYTHONPATH=bindings/out/python python3 -c "
import luma_core as g
print(g.glasses_gatt())
p = g.GlassesParser()
print(p.push(bytes.fromhex('ac550009550104080103010269')))"
```

`python/demo.py` is a ~110-line [bleak](https://github.com/hbldh/bleak) client that scans
for the `AA12` service, subscribes to both notify characteristics, writes the connect
interrogation and prints every decoded event:

```bash
pip install bleak
PYTHONPATH=bindings/out/python python3 python/demo.py
```

It has not been run here — there is no hardware in this environment.

## iOS demo app

`ios/` is a one-screen SwiftUI app (`luma.core.demo`, iOS 17+) that scans, connects,
subscribes to `AA14`/`AA15`, runs the connect interrogation and renders every decoded
event. It reads the UUIDs from `glassesGatt()` and builds every frame with the generated
`glasses*` functions — the app owns CoreBluetooth and nothing else.

```bash
./ios/build-core.sh                 # REQUIRED FIRST — see below
cd ios && xcodegen                  # only if you edited project.yml
open ios/LumaDemo.xcodeproj      # then run on a physical iPhone
```

**`ios/build-core.sh` must run before the project will build.** It cross-compiles the crate
for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, generates the Swift bindings into the
local SPM package at `ios/LumaCore/`, and copies a static archive per SDK into
`ios/lib/`. All of that output is gitignored, so a fresh clone has to run it. It sets
`DEVELOPER_DIR` to the full Xcode if unset, because `xcode-select` often points at the
Command Line Tools, whose sysroot is macOS-only.

**The Simulator has no Bluetooth radio.** The app builds and its UI can be inspected there,
but a real connection needs a paired physical iPhone.

```
ios/
  project.yml                       xcodegen source of truth; the .xcodeproj is committed
  build-core.sh                     cargo → static archives + generated Swift
  LumaDemo/LumaDemoApp.swift  the @main entry
  LumaDemo/GlassesLink.swift     all the CoreBluetooth, ~350 lines
  LumaDemo/ContentView.swift     status header, device list, event log, two buttons
  LumaCore/                  local SPM package holding the generated bindings
  lib/                              generated static archives (gitignored)
```

`OTHER_LDFLAGS: -lluma_core` and the per-SDK `LIBRARY_SEARCH_PATHS` live at PROJECT
level in `ios/project.yml`, not target level: at target level the app links clean while any
test bundle fails on undefined `uniffi_luma_core_*` symbols, which reads as a test
bug and is not one. It is a plain `.a` per SDK plus generated Swift — no `.xcframework`,
no SPM `.binaryTarget`.

## Deliberately not in this crate

This is a byte layer. It makes decisions and does arithmetic; it opens no socket, holds no
radio and reads no clock. `PROTOCOL.md` documents these; implementing them is the client's:

* **The SoftAP join** (§8) — `NEHotspotConfiguration` / `WifiNetworkSpecifier` work.
* **The JSON file API** (§9) — plain HTTP against the glasses' own AP.
* **RTSP and H.264 live** (§10) — sockets, an RTP jitter buffer, a hardware decoder.
* **Opus decode** (§6) — the crate parses the TOC byte and hands you the packet plus its
  geometry; turning bytes into PCM is a platform codec.
* **Every timing duration** — the ~2 s SSID wait, the ~1.2 s voice-idle gap, the 30 s
  open-mic cap, the poll interval, the reconnect ladder. This crate can NAME the moment a
  timer expires and cannot measure one.

## Evidence and provenance

Reverse-engineered from BLE and Wi-Fi packet captures of hardware in hand, and from the
vendor apps' own uploaded debug logs. **No vendor affiliation, no vendor code, and no
vendor NDA.** The vendor's proprietary interaction spec (三端交互协議 v07) is *not*
included and was used only as a claim to check against — this document contradicts it in
several places, and says so each time.

The captures themselves are not distributed. What survives of them is the golden vectors
in `src/frame.rs`, `src/reassembly.rs` and `src/voice.rs`: real frames, byte for byte, each
with the capture it came from and what it establishes. Every test asserts against those
rather than against a re-implementation, and there is no record mode — an expectation here
is derived and reviewed, never captured from the code it checks.

The library half was extracted from a private monorepo's sans-IO core with its tests
intact, then scrubbed of the internal framing. Nothing about the protocol was rewritten in
the move.

## Licence

MIT. See [LICENSE](LICENSE).
