# Luma Glasses — BLE Protocol Reference

The wire protocol between a phone (or any BLE central) and Luma smart glasses, as
implemented by [`luma-core`](README.md). This document is the human-readable half; the
crate is the executable half, and §20 maps one onto the other.

**Verified against:** E09 hardware, project `T1` / customer `0303`, firmware
**bt 1.4.8 / isp 1.3.1 / hw 2**. Other firmware may differ; the statuses below tell you
how much to trust each row.

**Status legend, used throughout:**

| | |
|---|---|
| ✅ | Verified in real sessions with the glasses |
| 🧪 | Probed directly against the hardware |
| ⬜ | Defined by the platform, not yet exercised. Sendable, but expect nothing |

**Conventions.** Every multi-byte integer on this wire is **big-endian**. Settings
values are ASCII digits (`0x30` = `'0'`, `0x31` = `'1'`) except the record duration,
which is a 2-byte integer. Media and call commands use raw `0x00`/`0x01`. The phone time
is plain hex, not BCD. Numbers are not consistently encoded across commands, and the
decoders follow each field rather than a family rule.

---

## 1. Transport

One GATT service, one write characteristic, two notify characteristics.

| | UUID | Notes |
|---|---|---|
| service | `AA12` | advertise-filter on this to find the glasses |
| write, app → glasses | `AA13` | ATT **Write Request** (with response). Every write is acknowledged |
| control notify | `AA14` | all command replies, pushes and the voice stream |
| file notify | `AA15` | the `52 58` file stream only (§15) |

`AE00`, `AE01`, `AE02` and the standard Battery service (`180F`) are also present and are
not used by this protocol.

Subscribe to **both** notify characteristics before sending anything. The glasses push a
capabilities frame (§8) about 30 ms after the link comes up, before the app has written a
byte.

Use a write *with* response. A write-without-response is a different ATT opcode that the
glasses never acknowledge and gives you no flow control; on this firmware every write is
answered.

## 2. Frame format

```text
app → glasses   AB 55 | len (2, BE) | cmd (1) | data (N) | crc (1)
glasses → app   AC 55 | len (2, BE) | cmd (1) | data (N) | crc (1)

len = 1 (cmd) + N (data) + 1 (crc)          a whole frame is 4 + len bytes
crc = (cmd + Σ data) & 0xFF                 an additive checksum, not a polynomial CRC
```

A command with no payload carries one `0x00` filler byte, so the smallest frame is
7 bytes and `len` is never below 3.

```text
get battery      AB 55 00 03 17 00 17
battery reply    AC 55 00 05 17 3A 30 01 82      → 100 %, charging
```

`len` counts the command byte and the checksum but **not** the four header bytes; the
checksum covers the command byte and the data but **not** the header or the length. Both
boundaries are easy to get wrong in a way that only shows on non-trivial payloads.

**Receive as a stream.** In practice every frame arrives whole in its own notification,
but a receiver should still be a streaming demuxer: buffer, scan for the header, take
complete frames, keep the remainder. `Parser` and `Deframer` in the crate do exactly this,
so you can hand them notifications verbatim.

Most setters are echoed back verbatim as the acknowledgement.

## 3. Recommended connection sequence

1. Connect, discover `AA12`, subscribe to `AA14` and `AA15`.
2. Expect an unsolicited `0x95` capabilities push (§8).
3. Read in this order: `0x55` versions, `0x64` project name, `0x17` battery, `0x45`
   status, `0x48` switch states (a burst, §7), `0x69` volumes, `0x95` capabilities, `0x71`
   voice-feature state.
4. Send `0x59` phone time (§16).
5. Thereafter: battery arrives on its own via `0x53` (§5), so a poll of `0x17` every
   5–10 s is plenty; refresh `{0x45, 0x48, 0x69, 0x95}` every ~30 s or on demand.

## 4. Command reference

### App → glasses

| cmd | name | data | status | see |
|---|---|---|---|---|
| `0x01` | set LED brightness | `30`/`31`/`32` low/mid/high | ✅ | §18 — status indicator only, no off value |
| `0x02` | set record duration | 2 B BE seconds (`00 5A` = 90 s) | ✅ | §9 |
| `0x04` | wear detection | `30`/`31` | ✅ | §7 |
| `0x06` | voice command (wake word) | `30`/`31` | ✅ | §7 |
| `0x07`–`0x11` | gesture bindings, one slot each | ASCII digit | ✅ | §7 |
| `0x14` | factory reset | `00` | ⬜ | §16 |
| `0x17` | get battery | `00` | ✅ | §5 |
| `0x22` | take photo | `30` photo, `31` photo + AI image | ✅ | §9, §15 |
| `0x23` / `0x24` | start / stop video | `00` | ✅ | §9 |
| `0x30` | switch track | `00` previous, `01` next | ✅ | §11 |
| `0x31` | play / pause | `00` pause, `01` play | ✅ | §11 |
| `0x32` | volume step | `00` down, `01` up | ✅ | §11 — SYSTEM channel only |
| `0x33` | call | `00` hang up, `01` answer | ✅ | §11 |
| `0x34` | voice recording | `00` stop, `01` start | ✅ | §10 |
| `0x39` | open Wi-Fi for the file API | `30` AP, `31` P2P | ✅ | §12, §13 |
| `0x40` | get file count | `00` | ✅ | §9 — replies as `0x42` |
| `0x44` | Wi-Fi done / tear down | `30 00` all, `30 01` ISP off only, `31 <n>` partial | ✅ | §12 |
| `0x45` | get device status | `00` | ✅ | §6 |
| `0x48` | get all switch states | `00` | ✅ | §7 — replies as a burst |
| `0x55` | get versions | `00` | ✅ | §5 |
| `0x56` | interrupt voice | `00` | ✅ | §10 — the only way to close the mic |
| `0x57` | retransmit voice | `00` | ⬜ | §10 |
| `0x59` | send phone time | `YY MM DD HH MM SS`, hex | ✅ | §16 |
| `0x60` | reboot | `00` | ⬜ | §16 |
| `0x61` | camera orientation | `30` portrait, `31` landscape | ✅ | §9 |
| `0x62` | offline voice language | `00` zh, `01` en | ⬜ | §7 |
| `0x63` | enter firmware upgrade | `30` AP, `31` P2P | ⬜ | §16 |
| `0x64` | get project name | `00` | ✅ | §5 |
| `0x65` | send ISP version | 3 B | ⬜ | §16 |
| `0x67` | open Wi-Fi for live view | `30` AP, `31` P2P | ✅ | §12, §14 |
| `0x69` | get volumes | `00` | ✅ | §11 |
| `0x70` | set volume | `<channel> <level>` | ✅ | §11 |
| `0x71` | get voice-feature state | `00` | ✅ | §8 |
| `0x95` | get capabilities | `00` | ✅ | §8 |

### Glasses → app

| cmd | name | payload | status | see |
|---|---|---|---|---|
| `0x25` | Wi-Fi network name | NUL-terminated SSID | ✅ | §12 |
| `0x42` | file count | 2 B BE | ✅ | §9 |
| `0x45` | status (action sync) | 10 flags | ✅ | §6 |
| `0x46` | voice data | one Opus packet | ✅ | §10 |
| `0x49` | voice abandoned | | ⬜ | §10 |
| `0x51` | AI broadcast cancelled | | ⬜ | |
| `0x52` | HD image failed | | ⬜ | §15 |
| `0x53` | battery push | `<charging> <percent>` raw | ✅ | §5 |
| `0x96` | ISP upgrade done | | ⬜ | §16 |
| `0x97` | voice stream start | | ✅ | §10 |
| `0x99` | voice stream end | | ⬜ | §10 — never sent by this firmware |

Plus the echo of every setter, and the per-setting frames of the `0x48` burst (§7).

## 5. Device information

**`0x55` versions** → 7 bytes: `bt(3) isp(3) hw(1)`, e.g. `01 04 08 01 03 01 02` =
bt 1.4.8, isp 1.3.1, hw 2.

**`0x64` project name** → 8 ASCII bytes: 4 project + 4 customer (`T1` / `0303`).

**`0x17` battery** → `<tens> <ones> <charging>` where the digits are ASCII (`0x30 + n`).
At 100 % the tens digit overflows to `0x3A`; the crate decodes that as 100 rather than
freezing at 99. `charging` is raw `0x00`/`0x01`.

**`0x53` battery push** → `<charging> <percent>`, both raw bytes. Sent unsolicited
roughly every 10 s while discharging and every 60 s while charging. Note the two battery
messages use different encodings for the same quantity.

## 6. Device status — `0x45`

The reply carries **ten** one-byte flags:

| idx | flag | fires when |
|---|---|---|
| 0 | `takePhoto` | a photo is being captured |
| 1 | `audioRecord` | voice recording (`0x34`) is active |
| 2 | `videoRecord` | video (`0x23`) is active |
| 3 | `volumeUp` | touchpad swipe |
| 4 | `volumeDown` | touchpad swipe |
| 5 | `nod` | never observed to fire |
| 6 | `shakeHead` | never observed to fire |
| 7 | `playPause` | single tap |
| 8 | `wearingDetection` | never observed to fire |
| 9 | `importingMode` | a Wi-Fi session is open (§12) |

Index 9 means "file-import mode", which is how the glasses describe having their Wi-Fi up.

## 7. Settings and the switch-state read — `0x48`

Each setting has its own setter (`0x01`, `0x02`, `0x04`, `0x06`, `0x07`–`0x11`, `0x61`),
echoed back as the acknowledgement.

**Reading them all with `0x48` returns a burst, not a single frame.** The glasses answer
with one frame *per setting*, keyed by that setting's own setter opcode, in this order:

```text
AC 55 00 03 01 31        LED brightness    '1'
AC 55 00 04 02 00 3C     record seconds    0x003C   ← 2-byte integer, not ASCII
AC 55 00 03 04 31        wear detection    '1'
AC 55 00 03 06 31        voice command     '1'
AC 55 00 03 07 30        gesture slot      ┐
AC 55 00 03 08 31        gesture slot      │
AC 55 00 03 09 32        gesture slot      │  five slots
AC 55 00 03 10 34        gesture slot      │
AC 55 00 03 11 33        gesture slot      ┘
AC 55 00 03 61 30        orientation       '0'
```

There is **no `0x48` reply frame**; a client that waits for one waits forever. The crate's
`SwitchStates` accumulator collects the burst and reports when all ten have landed.

**Gesture slots.** Slot ids `0x07`–`0x11` are decimal-looking (7–11), not hex 16/17. The
value is the bound action:

| slot | gesture | default value → action |
|---|---|---|
| `0x07` | swipe forward | `0` volume down |
| `0x08` | swipe back | `1` volume up |
| `0x09` | single tap | `2` play / pause |
| `0x10` | double tap | `4` previous track |
| `0x11` | triple tap | `3` next track |

The slot-to-gesture assignment matches the factory defaults; it has not been confirmed by
rebinding a gesture and watching which slot changes (§19).

`0x62` offline voice language is never reported in the burst.

## 8. Capabilities — `0x95` — and voice-feature state — `0x71`

Both are bitmaps.

**`0x95`** → 4 bytes big-endian. Observed value `00 00 04 0B`. The flags it expands to are:
`aiAwaken`, `offlineVoice`, `debounce`, `debounceDynamic`, `photoMode`,
`photoModeDynamic`, `factoryDataReset`, `voiceAdjust`, `languageSwitchDynamic`,
`waterMask`, `live`, `wearingDetection`, `wearingDetectionDynamic`. On the verified unit
`photoModeDynamic`, `voiceAdjust`, `live` and `wearingDetectionDynamic` are true. The
bit-to-flag assignment is not yet pinned (§19), so the crate exposes the raw word.

The glasses push `0x95` unsolicited about 30 ms after every link-up.

**`0x71`** → 1 byte: which voice features are *disabled* (`aiAwaken`, `offlineVoice`).
Only `0x00` has been observed, so bit positions are unknown; the crate exposes the raw
byte. `0x95` and `0x71` are a pair: what the glasses can do versus what is switched off.

## 9. Camera

| | |
|---|---|
| `0x22 30` | take a photo. Saved to the `EVENT` folder (§13) |
| `0x22 31` | take a photo **and** push a small copy over BLE for AI use (§15) |
| `0x23` / `0x24` | start / stop a video clip, saved to `LOOP` |
| `0x02` | clip length in seconds, 2 B BE. 60, 90, 180, 300, 420 and 600 are all in use |
| `0x61` | orientation, `30` portrait / `31` landscape |
| `0x40` → `0x42` | number of files waiting, 2 B BE. The reply comes as `0x42`; `0x40` is never echoed |

Full-resolution photos (~300–400 KB) and videos only leave the glasses over Wi-Fi (§13).

## 10. Voice

Two independent things share the name.

**Voice recording (`0x34`)** starts and stops an on-device recording that lands in the
`AAC` folder (§13). Nothing streams to the phone.

**The wake-word stream** is what a voice assistant uses. When the on-device wake word
fires (or `0x06` voice command is enabled and triggered), the glasses send:

```text
AC 55 … 97 …          stream start
AC 55 … 46 <40 B>     one Opus packet, about 50 per second, on AA14
AC 55 … 46 <40 B>
…
```

Each `0x46` payload is one complete **Opus** packet: SILK mode, wideband, one 20 ms
frame, mono. Request 16 kHz output from your decoder. The crate parses the packet header
(`OpusToc`) and hands you the packet plus its geometry; decoding to PCM is a platform
codec (CoreAudio on iOS, `MediaCodec` on Android, libopus elsewhere).

**The stream does not end on its own.** `0x99` (stream end) is defined but this firmware
never sends it; the glasses keep streaming for as long as the link is up. The microphone
closes **only** when the app writes `0x56`. So the client owns two timers, and must write
`0x56` unconditionally when either fires:

| timer | value that works |
|---|---|
| idle gap: no `0x46` for this long means the user stopped talking | ~1.2 s |
| hard cap on one capture | ~25–30 s |

Writing `0x56` twice ~0.5 s apart is harmless and is what shipping clients do.

`0x57` retransmit and `0x49` voice-abandoned are defined and unexercised.

## 11. Audio, media and calls

| | |
|---|---|
| `0x30` | previous (`00`) / next (`01`) track |
| `0x31` | pause (`00`) / play (`01`) |
| `0x33` | hang up (`00`) / answer (`01`) |
| `0x69` | get volumes → 3 raw bytes `[system, media, call]`, observed range `0x05`…`0x10` |
| `0x70` | set volume `<channel> <level>`, channel `00` system / `01` media / `02` call |
| `0x32` | volume step down (`00`) / up (`01`) |

**`0x32` only moves the SYSTEM channel.** The glasses' own touchpad swipes move MEDIA.
For music playback use `0x70` with an explicit channel; relative steps are the wrong
control. Volume levels are a raw scale carried verbatim; the crate does not normalise
them.

Note the encodings: settings use ASCII digits, media and call commands use raw `0`/`1`.
`voice_command(on)` sends `0x31`; `play_pause(play)` sends `0x01`. The builders in the
crate get this right so you do not have to.

## 12. Wi-Fi

The glasses raise their own access point on demand. Two commands bring up the **same**
network and differ only in what the glasses serve on it:

| command | serves |
|---|---|
| `0x39` | the file API (§13) |
| `0x67` | the RTSP live stream (§14) |

Sequence:

1. Write `0x39 30` or `0x67 30` (AP mode; `31` selects P2P, which is untested).
2. The glasses acknowledge, then push `0x25` with the SSID (e.g. `DH-TwI-4641E4`) about
   2.5 s later. The passphrase is fixed: **`12345678`**.
3. Wait **~2 s after the SSID arrives** before joining; the AP is not ready sooner.
4. Join. The glasses are **`192.168.169.1`**; the phone gets `192.168.169.100` by DHCP.
5. Do the work (§13 or §14).
6. Write `0x44` to tear down: `30 00` all done, `30 01` power off the image processor
   only, or `31 <n>` after a partial download. Then drop the network so the phone returns
   to its normal Wi-Fi.

While the AP is up, `0x45` flag 9 (`importingMode`) reads 1.

**The join is flaky by nature.** Expect retries and 25 s timeouts from the OS, and iOS
occasionally bouncing back to the previous network. Telling the user to switch off
cellular data for the duration helps on iOS.

## 13. File API (over Wi-Fi, after `0x39`)

A small JSON-over-HTTP API on `http://192.168.169.1`:

| | |
|---|---|
| list | `GET /app/getfilelist` |
| thumbnail | `GET /app/getthumbnail?file=<FOLDER>/<name>` |
| download | `GET /<FOLDER>/<name>` |
| delete | `GET /app/deletefile?file=<FOLDER>/<name>` |

```json
{"result":0,"info":[
  {"folder":"EVENT","files":[
    {"name":"EVENT/20260727225147720.jpg","size":378,
     "createtimestr":"20260727225146","type":1}],"count":2},
  {"folder":"AAC","files":[],"count":0},
  {"folder":"LOOP","files":[],"count":0},
  {"folder":"EMR","files":[],"count":0}]}
```

Four fixed folders: **EVENT** photos, **AAC** voice recordings, **LOOP** video clips,
**EMR** emergency (always empty so far). `name` already carries the folder prefix. Delete
replies `{"result":0,"info":"success."}`. Files are timestamped in their names; the
glasses keep their own clock from `0x59` (§16).

> **`size` is in KiB, not bytes.** `size: 378` downloads as 387,972 bytes; in every sample
> `size == floor(bytes / 1024)`. A byte-exact truncation check against `size` rejects every
> download.

A sync loop that works: list → fetch thumbnails → for each file, download then delete,
pipelined one behind → `0x44 30 00`.

## 14. Live view (over Wi-Fi, after `0x67`)

Connect straight to RTSP; there is no HTTP handshake and no BLE traffic in between.

```text
rtsp://192.168.169.1:554/h264          Server: Hisilicon RTSP Streaming Media Server/1.0.0
OPTIONS → DESCRIBE → SETUP track0 → PLAY

m=video 0 RTP/AVP 96      a=rtpmap:96 H264/90000       a=control:track0
m=audio 0 RTP/AVP 97      a=rtpmap:97 mpeg4-generic/16000/1
                          a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408;
                                    sizeLength=13; indexlength=3; indexdeltalength=3
Transport: RTP/AVP;unicast;client_port=8712-8713
```

- **H.264 Main profile, level 5.2, 1600×1200**, SPS/PPS in-band (STAP-A), slices as FU-A,
  dynamic payload type 96.
- Frame rate is not signalled. Measured ~25 fps at ~3.3 Mbps on a clean link.
- The AAC-LC 16 kHz mono audio track is advertised and works when requested (RFC 3640
  AAC-hbr); treat it as best-effort and never let a refused audio `SETUP` break video.
- Use a hardware decoder (`VideoToolbox`, `MediaCodec`). A software decode of 1600×1200
  drowns in jitter-buffer overruns.

## 15. The AI image — `52 58` file stream on `AA15`

After `0x22 31`, the glasses push a small JPEG (~11 KB) over BLE using a **second
framing** on the file characteristic:

```text
info   52 58 | len (2, BE) | 97 | total (4 BE) type (1) | crc | 58 52
data   52 58 | len (2, BE) | 98 | addr  (4 BE) bytes…   | crc | 58 52
end    52 58 | len (2, BE) | 99 | 00                    | crc | 58 52

len = 1 (cmd) + payload + 1 (crc)      a whole frame is 4 + len + 2 bytes
crc = (cmd + Σ payload) & 0xFF         the same additive checksum as §2
```

A real transfer:

```text
→ AB 55 00 03 22 31 53                        take photo + AI image
← AC 55 00 03 22 01 23                        ack
← 52 58 00 07 97 00 00 2A D0 02 93 58 52      info: 10,960 bytes, type 0x02 = HD image
← 52 58 01 F6 98 <addr> <496 bytes> <crc> 58 52   × 23 frames, addresses 0x000000 → 0x002AA0
```

Chunks are 496 bytes with a shorter last one (22 × 496 + 48 = 10,960). Throughput is
about 13.7 kB/s, so the image lands ~0.8 s after the ack; capture itself takes ~2.4 s.
Note that these opcodes (`0x97`, `0x99`) collide by value with the voice-stream opcodes
on `AA14`; the framing and the characteristic keep them apart. Feed `AA15` bytes to the
crate's `FileReassembler` and wait for `FileEvent::Completed`.

Full-resolution photos do **not** travel this way; use the file API (§13).

## 16. Time and maintenance

**`0x59` phone time** → `YY MM DD HH MM SS` as plain hex bytes (`1A 07 1B 12 28 12` =
2026-07-27 18:40:18). **Not BCD**: a BCD encoder sets the glasses' clock to a garbage
date. The glasses date their media from this clock. Send it once per connection.

`0x14` factory reset, `0x60` reboot, `0x63` enter upgrade, `0x65` send ISP version and
the `0x96` upgrade-done reply are defined and unexercised (⬜).

## 17. Quirks and gotchas, collected

- `0x48` never answers with a `0x48` frame; it answers with ten per-setting frames (§7).
- The wake-word stream never ends by itself; `0x56` from the app is the only close (§10).
- `0x40` is never echoed; the count comes as `0x42` (§9).
- `0x17` battery is ASCII digits and overflows to `0x3A` at 100 %; `0x53` is raw (§5).
- `0x02` record duration is a 2-byte integer inside a burst of ASCII digits (§7).
- `0x59` is hex, not BCD (§16).
- `0x32` moves only the system volume (§11).
- File-API `size` is KiB (§13).
- Wait ~2 s after the SSID before joining the AP (§12).
- Voice packets and control replies share `AA14`; the file stream is alone on `AA15` (§1).
- Every multi-byte field is big-endian (conventions, above).

## 18. Reserved and non-functional commands

**`0x01` LED brightness** controls the brightness of the firmware-driven status indicator.
There is no off value, no torch and no flash; nothing user-facing is gained by exposing it.

**Defined by the platform, never seen to do anything:** `0x03` microphone, `0x16`
capacity, `0x35` camera style, `0x36`/`0x37` image pull, `0x43` gesture recover, `0x50`
camera mode. The crate lists them in `VENDOR_NAMED_UNIMPLEMENTED` and deliberately does
**not** give them `AppCommand` variants, so they cannot be sent by accident.

**`0x36` image pull (🧪).** The firmware recognises it and replies within ~30 ms with an
echo of the index byte, for any index including out-of-range ones, and then sends **no
data**. `0x37` gets no reply at all. There is no BLE path to a full-resolution photo on
this firmware; use Wi-Fi.

If you probe an unknown opcode yourself: log inbound frames while you do it, vary the
payload (a reply that tracks the payload is an echo, not a status), and send a bogus
opcode such as `0xEE` as a control so you know what "unrecognised" looks like on the unit
in front of you. Silence in your own log is not silence on the wire.

## 19. Open questions

- Gesture slot ↔ gesture assignment (§7): confirm by rebinding one gesture.
- `0x95` and `0x71` bit positions (§8): needs a unit with different capabilities, or a
  voice feature switched off.
- The ⬜ rows in §4.

## 20. Where each part lives in the crate

| section | module | types and functions |
|---|---|---|
| §1 transport | `gatt` | `SERVICE`, `WRITE`, `CONTROL_NOTIFY`, `FILE_NOTIFY`, `NOTIFY_ALL`, `WRITE_WITH_RESPONSE`, `PRESENT_BUT_UNUSED`, `is_drivable` |
| §2 frame format | `frame` | `encode`, `encode_command`, `decode_device`, `checksum`, `Frame`, `DecodeError`, `Deframer`, `Residual` |
| §4 command table | `opcodes` | `AppCommand`, `DeviceUpload`, `Evidence`, `VENDOR_NAMED_UNIMPLEMENTED` |
| §4 builders | `commands` | one function per row: `take_photo`, `set_volume`, `send_phone_time`, `get_switch_states`, … |
| §5 device info | `parser` | `Versions`, `Identity`, `BatteryReading`, `BatterySource` |
| §6 status | `parser` | `ActionSync`, `DeviceAction` |
| §7 settings | `parser`, `opcodes` | `SwitchStates`, `SwitchReport`, `GestureSlot`, `GestureAction`, `Orientation`, `SWITCH_STATE_BURST_ORDER` |
| §8 capabilities | `parser` | `Capabilities`, `VoiceFeatures` |
| §9 camera | `commands`, `parser` | `take_photo`, `start_video`, `set_record_duration`, `MediaCount` |
| §10 voice | `voice` | `VoiceStream`, `VoicePacket`, `VoiceEvent`, `EndCause`, `OpusToc` |
| §11 audio | `commands`, `parser` | `set_volume`, `VolumeChannel`, `Volumes` |
| §12 Wi-Fi | `commands`, `parser` | `open_wifi`, `WifiService`, `WifiCredentials` (`ssid`, `passphrase()`), `file_download_complete` |
| §15 file stream | `reassembly` | `FileReassembler`, `FileEvent`, `ReassembledFile`, `FileKind`, `decode_file_frame` |
| all inbound | `parser` | `Parser::push(bytes) -> Vec<DeviceEvent>` |
| other languages | `ffi` (feature `uniffi`) | `GlassesParser`, `GlassesDeframer`, `GlassesFileReassembler`, `GlassesVoiceStream`, `GlassesSwitchStates`, the `glasses_*` functions |

### Beyond the sans-IO core

The core makes decisions and does arithmetic; it opens no socket, holds no radio and reads
no clock. The parts that need I/O are in the same repository, behind feature flags, so a
laptop can run the whole protocol with nothing else:

| | sans-IO (default build) | with I/O |
|---|---|---|
| BLE (§1) | `gatt`, `frame`, `parser`, `commands` | feature `ble`: `client::ble::Glasses` (btleplug), `examples/luma.rs` |
| file API (§13) | `fileapi`: URLs, listing and reply parsers, the KiB rule, `SyncPlan` | feature `wifi-client`: `client::fileapi::FileClient` (ureq), `examples/gallery.rs`, `python/gallery.py` |
| live view (§14) | `rtsp`, `sdp`, `rtp`, `h264`, `aac` | `examples/live.rs` (std sockets), the iOS demo's live screen |
| voice decode (§10) | `voice`: packet framing and Opus headers | feature `opus`: `client::opus::VoiceDecoder` + WAV writer |
| durations | `timing`: every recommended value as a constant | your timers |

The Wi-Fi *join* is the one step that stays platform-specific; the [guide](docs/GUIDE.md)
gives the call for each OS.
