# Smart-glasses protocol — reverse-engineered reference

Everything here is **device-confirmed** from BLE packet captures, Wi-Fi packet
captures, and the vendor apps' own uploaded debug logs. It supersedes the
vendor's own interaction spec (三端交互协议 v07 — a proprietary PDF, not
included here) wherever the two disagree, and they disagree a lot.

Hardware under test: an `E09` unit, project `T1` / customer `0303`
(`equipmentCode T10303`), bt **V1.4.8** / isp **V1.3.1** / hw **V2**.

Two vendor apps drive this same firmware and were captured side by side:
**EyeVue** 3.2.7 and **GlassX** 3.0.8. Both are skins on the Watchfun
platform (`WF_DanHaiBLEManager`, `SdkCode: neomix`), which is why they share
opcodes but differ in which ones they bother to send.

Captures are referred to by name throughout — `eyevue_1`…`eyevue_4`,
`glassx_1`, `client_1`…`client_3`. The `eyevue_*`/`glassx_*` traces are the two
vendor apps driving the hardware; the `client_*` traces are the reference client
this crate was extracted from driving the same unit. The captures themselves are
not distributed; the golden vectors in `src/frame.rs`, `src/reassembly.rs` and
`src/voice.rs` carry the bytes that matter, verbatim.

---

## 1. How to capture this yourself

Three channels, in ascending order of effort and descending order of yield.

**The vendor apps' own debug logs.** This is the single highest-value channel
and it is not packet capture. Both vendor apps record every BLE frame in both
directions **with the app's own decoded interpretation**, then upload the whole
log to object storage and announce its URL in a plaintext HTTP POST. That
decoded interpretation is how `0x95`, `0x71` and `0x67` were identified — a raw
capture shows you the bytes, the vendor's log tells you what the vendor calls
them. The apps flush their log **on backgrounding**, so swipe the app away
after each block of testing or nothing is written.

**iOS — PacketLogger.** Install Apple's *Bluetooth* profile on the iPhone
(`developer.apple.com/bug-reporting/profiles-and-logs/`, reboot after
installing), reproduce, then pull the `.pklg` off the device with PacketLogger
from Additional Tools for Xcode. This gives you HCI with ATT decoded, which is
what every capture behind this document is. Read it with
`tshark -r <capture>.pklg -Y btatt.value -T fields -e btatt.value`, or open it
in Wireshark.

**Android — btsnoop.** Enable *Developer options → Enable Bluetooth HCI snoop
log*, restart the Bluetooth stack (or the phone), reproduce, then pull
`/data/misc/bluetooth/logs/btsnoop_hci.log` via a bug report. Confirm the
setting actually took before trusting a session — several OEM builds silently
keep the old value until the stack restarts.

For the Wi-Fi side (§8–§10) join the glasses' SoftAP from a laptop and capture
in monitor mode, or run the HTTP calls yourself against `192.168.169.1` and
capture on the laptop's own interface.

---

## 2. GATT

| | |
|---|---|
| service | `AA12` |
| write (app → device) | `AA13`, Write **Request** (0x12, with response) |
| control notify | `AA14` |
| file/voice notify | `AA15` |

`AE00`/`AE01`/`AE02` and the standard Battery service are present but **neither
vendor app touches them**. Voice PCM and control replies both arrive on `AA14`
on this firmware; `AA15` carries only the `52 58` file stream.

Frames:

```
app → device   AB 55 | len(2, BE) | cmd | data… | crc
device → app   AC 55 | len(2, BE) | cmd | data… | crc
len = 1 (cmd) + N (data) + 1 (crc)
crc = (cmd + Σ data) & 0xFF
```

A command with no payload carries a single `0x00` filler byte. Frames are
both fragmented across notifications and coalesced several-per-notification —
the receiver must be a streaming demuxer, not one-frame-per-packet.

---

## 3. Command table

Opcodes marked ★ are **absent from the v07 PDF** — its app→device list stops
at `0x65`.

### App → device

| cmd | name | data | notes |
|---|---|---|---|
| `0x01` | setLED | `30`/`31`/`32` | low/mid/high. **No off value exists** — see §3a. Not exposed in the app |
| `0x02` | setRecordDuration | 2 B BE seconds | `00 5A`=90 s, `00 B4`=180 s. GlassX offers 1/3/5/7/10 min |
| `0x04` | wearDetection | `30`/`31` | |
| `0x06` | voiceCommand | `30`/`31` | |
| `0x07`–`0x11` ★ | gesture bindings | ASCII digit | see §5 |
| `0x14` | factoryReset | `00` | |
| `0x17` | getBattery | `00` | reply `<tens><ones><charging>`, digits are `0x30+n` — at 100 % it sends `3A 30` |
| `0x22` | takePhoto | `30` photo, `31` photo **+ HD image for AI** | |
| `0x23` / `0x24` | start / stop video | `00` | |
| `0x30` | switchMusic | `00` prev, `01` next | |
| `0x31` | playPause | `00` pause, `01` play | |
| `0x32` | volume | `00` down, `01` up | relative |
| `0x33` | answerHangup | `00` hang up, `01` answer | |
| `0x34` | voiceRecording | `00` stop, `01` start | |
| `0x39` | **openWiFi: gallery** | `30` AP, `31` p2p | brings up the SoftAP for the **file API** |
| `0x40` | getFileCount | `00` | replies via `0x42`, never echoes `0x40` |
| `0x44` | fileDownloadComplete | `30 00` all done / `30 01` ISP off only / `31 <n>` partial | teardown for **both** Wi-Fi modes |
| `0x45` | getDeviceStatus | `00` | reply = action-sync, §4 |
| `0x48` | getSwitchStates | `00` | reply is a **burst**, §5 |
| `0x55` | getVersions | `00` | `bt(3) isp(3) hw(1)` |
| `0x56` | interruptVoice | `00` | **the only thing that closes the mic**, §6 |
| `0x57` | retransmitVoice | `00` | never observed |
| `0x59` | sendPhoneTime | `YY MM DD HH MM SS` (hex, not BCD) | |
| `0x60` | reboot | `00` | never observed |
| `0x61` | orientation | `30` portrait, `31` landscape | |
| `0x62` | offlineVoiceLang | `00` zh, `01` en | never observed on the wire |
| `0x63` | enterUpgrade | `30` ap, `31` p2p | never observed |
| `0x64` | getProjectName | `00` | 8 B: 4 project + 4 customer ASCII |
| `0x65` | sendISPVersion | 3 B | never observed |
| `0x67` ★ | **openWiFi: live** | `30` AP, `31` p2p | brings up the SoftAP for **RTSP**, §8 |
| `0x69` ★ | getVolumes | `00` | reply `[system, media, call]` |
| `0x70` ★ | setVolume | `<channel> <level>` | channel `00` system / `01` media / `02` call |
| `0x71` ★ | getVoiceDisableState | `00` | 1-byte bitmap, §7 |
| `0x95` ★ | getCapabilities | `00` | 4-byte BE bitmap, §7 |

### 3a. `0x01` LED — dead on arrival

Chased through the vendor **Android** APK (decompiled with jadx) after neither
iOS app was ever seen sending it. The result is conclusive
enough to drop the feature:

* `z39.java` (`SendCommandViaBle`) implements `setLedBrightness(int)` →
  `configSendCommand(1, i)`, which builds `AB 55 00 03 01 <v> <crc>`. So the
  opcode is real and the frame shape is confirmed.
* **It has zero callers.** Two grep hits in 17,989 decompiled files: the
  declaration and its abstract in `w34.java`. Nothing invokes it, and there is
  no reflection path.
* **No LED level constants.** `yf1.java` (`Command`) holds the app's constant
  table; its `48/49/50` runs all belong to gestures and camera direction. There
  is no LED triple and no fourth value anywhere.
* **No LED UI.** `GlassesSettingsFragment` enumerates its entire menu:
  `DEVICE_INFO, OTA, QUICK_VOLUME, LIVE, CAMERA_DIRECTION, VIDEO_LENGTH`.
* **The app does not even parse it on receive** — `ModBleResponse`'s cmd switch
  has no `case 1:`, so the `0x01` frame inside the `0x48` burst is dropped.
* **No LED bit in the `0x95` capability word** (`ck2.java` enumerates all 13).
* The only light strings in the whole resource table (all locales) are pairing
  copy about the **指示灯 / indicator light**. No 补光 / 手电 / torch / fill-light
  vocabulary exists, and there is no camera-flash opcode.

Conclusion: the LED is a firmware-driven **status indicator**, `0x01` only
retunes its brightness, and no vendor client has ever driven it. The encoder would happily emit any byte, so a fourth value
can only be ruled in or out by probing the hardware — not from any client.

### Device → app

| cmd | name | notes |
|---|---|---|
| `0x25` | wifiName | NUL-terminated SSID; password is the fixed `12345678` |
| `0x42` | thumbnailCount | 2 B BE |
| `0x45` | actionSync | §4 |
| `0x46` | voiceData | PCM, ~50/s |
| `0x49` | abandonVoice | never observed |
| `0x51` | cancelAIBroadcast | never observed |
| `0x52` | hdImageFailed | never observed |
| `0x53` | chargeBattery | `<charging> <percent>`; pushed ~10 s discharging / ~60 s charging |
| `0x96` | ispUpgradeDone | never observed |
| `0x97` | voiceUploadStart | |
| `0x99` | voiceUploadEnd | **never sent by this firmware** — do not wait for it |

Most setters are echoed back verbatim by the device as an ack.

---

## 4. `0x45` action sync — 10 bytes

The PDF says 9 fields; the device sends 10. The vendor's own names
(from its `收到设备动作同步状态` dictionary):

| idx | name | confirmed by |
|---|---|---|
| 0 | `takePhoto` | photo capture |
| 1 | `audioRecord` | `0x34` |
| 2 | `videoRecord` | `0x23`/`0x24` |
| 3 | `volumeUp` | touchpad swipe (capture `eyevue_3`) |
| 4 | `volumeDown` | touchpad swipe (capture `eyevue_3`) |
| 5 | `nutationHead` (nod) | never fired |
| 6 | `shakeHead` | never fired |
| 7 | `playPause` | single tap |
| 8 | `wearingDetection` | never fired |
| 9 | `importingMode` | goes 1 while a Wi-Fi session is open |

Index 9 is **not** "Wi-Fi active" — the vendor calls it file-import mode.

---

## 5. `0x48` switch states — a burst, not a frame

The firmware does **not** answer `0x48` with an `AC 55 … 48 …` frame. It
emits one frame **per setting**, keyed by that setting's own setter opcode,
in the order the PDF §19 enumerates them:

```
ac 55 00 03 01 31      LED           = '1'
ac 55 00 04 02 00 3c   record secs   = 0x003C  ← 2-byte BE, NOT ASCII
ac 55 00 03 04 31      wear detect   = '1'
ac 55 00 03 06 31      voice command = '1'
ac 55 00 03 07 30      gesture slot  ┐
ac 55 00 03 08 31      gesture slot  │ PDF §19 item 5, 滑条手势
ac 55 00 03 09 32      gesture slot  │ (five slots)
ac 55 00 03 10 34      gesture slot  │
ac 55 00 03 11 33      gesture slot  ┘
ac 55 00 03 61 30      orientation   = '0'
```

Every value is an ASCII digit except `0x02`.

**Gesture slots — strong hypothesis, not yet proven.** The five values are
`0 1 2 4 3` on every capture, which matches the factory default printed on
p.2 of the PDF exactly (swipe-fwd = vol−, swipe-back = vol+, single tap =
play/pause, double tap = prev, triple tap = next):

| slot | gesture | value → action |
|---|---|---|
| `0x07` | swipe forward | `0` vol− |
| `0x08` | swipe back | `1` vol+ |
| `0x09` | single tap | `2` play/pause |
| `0x10` | double tap | `4` prev |
| `0x11` | triple tap | `3` next |

**To confirm:** change ONE gesture binding in the vendor app and see which
slot moves. No capture has done this yet.

Note `0x10`/`0x11` are decimal-looking ids (slots 7–11), not hex 16/17.

Spec §19 item 7 (offline voice language) is **never reported** by this
firmware.

---

## 6. Voice — the mic never closes on its own

Measured across every capture with voice in it:

| capture | 0x46 frames | gaps > 1.2 s | app sent 0x56 |
|---|---|---|---|
| eyevue_1 | 1,338 | none (26.7 s solid) | yes, at the end |
| eyevue_2 | 6,062 | 2, **each starting exactly at a 0x56** | yes ×3 |
| `client_1` | 3,982 | **none** (79.7 s solid) | **never** |
| `client_2` | 3,254 | **none** (65.1 s solid) | **never** |

The device never stops streaming and never sends `0x99`. **An app-written
`0x56` is the only thing that closes the mic.** The vendor's own open-mic
windows are 24.6 s and 29.6 s, and it writes `0x56` in **pairs** ~0.5 s apart.

Our `GlassesPacketParser.voiceStreamCap` (30 s) is calibrated to match.

---

## 7. `0x95` capabilities and `0x71` voice-disable

Both are bitmaps the app expands into named flags. The vendor logs
`payload大端序` (payload, big-endian) then dumps the map.

### `0x95` — 4 bytes big-endian

Observed value on this unit: `00 00 04 0B` (bits 0, 1, 3, 10 set).

```
supportAiAwaken = 0                supportOfflineVoice = 0
supportDebounce = 0                supportPhotoMode = 0
supportDebounceDynamic = 0         supportPhotoModeDynamic = 1
supportFactoryDataReset = 0        supportVoiceAdjust = 1
supportLanguageSwitchDynamic = 0   supportWaterMask = 0
supportLive = 1                    supportWearingDetection = 0
                                   supportWearingDetectionDynamic = 1
```

Four bits set, four flags true — but the map prints alphabetically, so the
**bit↔flag assignment is not pinned**. Needs a second unit with different
capabilities, or a firmware where a flag changes.

GlassX renders the same word as a UI layout:
`capability layout voice=true live=true photo=false cards=["voiceAdjust","live","videoDuration"]`.

The device also **pushes `0x95` unsolicited ~30 ms after every LE link-up**,
before the app writes anything (the CCCD persists across the bond).

### `0x71` — 1 byte

```
获取设备语音指令禁用状态stateMap：{ aiAwaken = 0; offlineVoice = 0; }
```

Which voice features are **disabled**. Only ever observed as `0x00`, so
bit positions are unknown. Sent in the connect/refresh group between `0x48`
and `0x55`.

`0x95` and `0x71` are a pair: what the device *can* do vs what is *switched off*.

### Not to be confused with the backend profile

`getInfoByDeviceCode` returns a much larger, server-side feature set
(`supportAi`, `supportTranslation`, `supportVideoTranslation`,
`supportSimultaneousInterpretation`, `supportVoiceNote`, `supportCloseLive`,
`supportLiveHorizontal`, `useInstructionCode: "video-00004"` …). That is an
app-feature profile, **not** the device bitmap.

---

## 8. Wi-Fi — two modes, two opcodes

Both `0x39` and `0x67` raise the **same** SoftAP (`DH-TwI-4641E4`, password
fixed `12345678`), report the SSID via `0x25` ~2.5 s later, and are torn down
with `0x44`. They differ in what the device serves:

| opcode | serves |
|---|---|
| `0x39` | the **file API** (§9) |
| `0x67` | the **RTSP live stream** (§10) |

GlassX sends `0x67` and never `0x39` when entering live mode; the gallery
path sends `0x39` and never `0x67`. The app waits ~2 s after the SSID arrives
before attempting the join (`T系列连接设备WiFi前延时2秒`) — the device needs
that long to actually raise the AP.

The phone gets `192.168.169.100` by DHCP; the device is **`192.168.169.1`**.
Neither of the PDF's addresses (`192.168.1.254`, `192.168.49.207`) is used by
this firmware.

**The join is genuinely flaky, for the vendor too.** Both apps log repeated
`NEHotspotConfiguration` retries, 25 s timeouts, and iOS bouncing back to the
original SSID (`检测到系统回连原始WiFi`). GlassX ships "temporarily turn off
cellular data" as user-facing advice.

---

## 9. File API — JSON REST, not the PDF's CGI

The PDF §3 documents a Novatek `?custom=1&cmd=NNNN` CGI API. **This firmware
does not implement it.** It serves:

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

Four fixed folders: **EVENT** photos, **AAC** voice recordings, **LOOP**
video, **EMR** emergency (always empty so far). `name` already carries the
folder prefix. Delete replies `{"result":0,"info":"success."}`.

> **`size` is in KiB, not bytes.** `size:378` downloaded as 387,972 bytes.
> Across six samples `size == floor(bytes / 1024)` exactly. A byte-for-byte
> truncation check against `size` rejects every download.

The vendor's sync order: flush leftover deletes from the previous session →
`getfilelist` → all thumbnails → then per file, download followed by its
delete, pipelined one behind. It never sets date or time over HTTP.

---

## 10. Live mode — RTSP / H.264

After `0x67` and the AP join, the app connects straight to RTSP. There is no
HTTP handshake and no BLE traffic in between.

```
rtsp://192.168.169.1:554/h264
Server: Hisilicon RTSP Streaming Media Server/1.0.0

OPTIONS → DESCRIBE → SETUP track0 → PLAY

m=video 0 RTP/AVP 96
  a=rtpmap:96 H264/90000
  a=decode_buf=300
  a=control:track0
m=audio 0 RTP/AVP 97
  a=rtpmap:97 mpeg4-generic/16000/1
  a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408;
            sizeLength=13; indexlength=3; indexdeltalength=3
  a=control:track1

Transport: RTP/AVP;unicast;client_port=8712-8713;server_port=53796-53797
```

* **H.264 Main profile, Level 5.2, 1600×1200.** SPS/PPS in-band via STAP-A,
  slices via FU-A. SSRC `0x7B7A7978`, dynamic payload type 96.
* Frame rate is **not** signalled in the SDP. Measured 25.3 fps @ 3.32 Mbps
  on a clean link; 21.8 fps @ 2.14 Mbps with 35 % packet loss on a bad one.
* **`track1` (AAC-LC 16 kHz mono) is advertised but never `SETUP` by either
  vendor app** — their live view is silent. It can be requested (RFC 3640
  AAC-hbr, `sizeLength=13 indexLength=3 indexDeltaLength=3`, `config=1408`)
  as a best-effort extra, but a refused audio SETUP must never break video.

GlassX plays it with FFmpeg (`User-Agent: Lavf60.3.100`) and a **software**
H.264 decoder, swscale `YUVJ420P → NV12`, into an `AVSampleBufferDisplayLayer`
sized 420×315. Its logs are wall-to-wall `jitter buffer full` /
`RTP: missed N packets`. VideoToolbox hardware decode is an easy win here.

---

## 11. AI vision — the image rides BLE

The photo for "what am I looking at" does **not** go over Wi-Fi. Full chain,
one real exchange:

```
← server  {"type":"transcribe","text":"我在看的东西是什么。","status":"recognized"}
← server  {"type":"visual_qa","query":"我在看的东西是什么","content":"visual_qa"}
→ BLE     ab 55 0003 22 31 53         cmd 0x22 data 0x31 = photo + HD image for AI
← BLE     ac 55 0003 22 01 23         ack
← BLE     52 58 0007 97 00002AD0 02 93 58 52
            └ file-info: 10,960 B, type 0x02 = HD image
← BLE     52 58 01F6 98 <addr(4,BE)> <496 B> <crc> 58 52   × 23 frames
            addresses 0x000000 → 0x002AA0, step 496 B, last frame 48 B
            22 × 496 + 48 = 10,960 ✓
→ server  {"command":"identifyImage",
           "base64Images":["data:image/jpeg;base64,/9j/4AAQ…"]}
← server  {"type":"realtimeChat","text":"看起来你正在看一个放在地上的黑色小箱子…"}
```

**The server asks for the image, not the app.** The app advertises
`isSupportPhoto:true` at session open; the backend decides the question needs
vision and sends `visual_qa` down the same socket.

The image is ~11 KB (not the ~300–400 KB the gallery downloads) and moves at
~13.7 kB/s over BLE. End to end: **5.76 s** from recognised question to answer
— 2.4 s capture, 0.8 s transfer, 2.3 s model.

The `52 58` framing is PDF §4.2/§4.3 verbatim, and `src/reassembly.rs`
implements it against three complete captured transfers.

---

## 12. Vendor cloud — out of scope

The vendor apps talk to a cloud assistant over their own WebSocket API, and the
glasses themselves never do: every byte the device sends or receives is on the
BLE and Wi-Fi links documented above. The endpoints, their auth and their
payloads are therefore out of scope for this crate and are not reproduced here.

## 13. Client guidance

Two findings that will bite a client if it does not know them. Both are
device-confirmed, and neither is in the vendor PDF.

**`0x32` only moves the SYSTEM channel.** Device-confirmed in capture
`client_1`: seven `0x32` writes walked `0x69` index 0 and left media untouched,
while the glasses' own touchpad moves MEDIA (capture `eyevue_3`). Neither vendor
app sends `0x32` at all. Relative volume buttons are therefore the wrong control
for music playback — use `0x70` with an explicit channel.

**`0x56` is the only thing that closes the mic.** `0x99` (voice-upload-end) is
declared by the PDF and by both reference implementations and is **never sent by
this firmware**: four captures totalling 14,636 `0x46` voice frames contain zero
of them, and two of those captures streamed for 79.7 s and 65.1 s solid. A
client that waits for `0x99` to close a capture waits forever. The mic closes
when the app writes `0x56`, and at no other time — so the client owns both the
idle timeout and the hard cap, and must write `0x56` unconditionally on the
runaway path. See §6.

Two smaller notes for anyone sizing a poll loop: the vendor apps poll
`{0x45, 0x17, 0x55, 0x95, 0x48, 0x69}` about every 10 s, and the indicator LED
(`0x01`, §3a) is not worth exposing — it is a firmware-driven status light and
no vendor client has ever driven it.

## 13a. Opcodes named by the Android APK, not yet seen on the wire

`z39.java` enumerates the vendor SDK's full app→device surface. It independently
confirms several of the findings above — **`103` = `0x67` is labelled *live***,
`149` = `0x95` is the support bitmap, `112` = `0x70` volume, `72` = `0x48`
device status — and names six commands no capture has produced:

| cmd | vendor name |
|---|---|
| `0x03` | microphone |
| `0x16` | capacity |
| `0x35` | camera style |
| `0x36` / `0x37` | image pull |
| `0x43` | gesture recover |
| `0x50` | camera mode |

None are implemented here. Listed so a future session does not re-derive them.

### 13b. `0x36` image pull — dispatched, acks, never delivers

**Device-tested 2026-07-28.** `0x36` is real: the firmware recognises it and
replies in ~30 ms with an echo of the byte sent.

| sent | reply | note |
|---|---|---|
| `0x36` index 0 | `AC 55 00 03 36 00 36` | echo `00` |
| `0x36` index 1 | `AC 55 00 03 36 01 37` | echo `01` |
| `0x36` index 2 | `AC 55 00 03 36 02 38` | echo `02` |
| `0x36` index 5 | `AC 55 00 03 36 05 3B` | echo `05` — **no range check** |
| `0x37` index 0 | *(silence)* | unimplemented |
| `0xEE`, `0xDE` (bogus control) | *(silence)* | echo is not a default path |

Three facts, each separately established:

1. **The reply echoes the index**, it is not a status code — the byte tracks
   whatever is sent.
2. **No data ever follows.** Zero `52 58` frames across four indices and 25 s
   of window, with AA15 confirmed subscribed (`setNotifyValue(true)` fires for
   both AA14 and AA15 at discovery), so this is a real negative.
3. **The echo is meaningful.** The `0xEE`/`0xDE` control proves the firmware
   drops commands it doesn't know; `0x36` is dispatched, `0x37` is not.

**Verdict: the ack exists, the data path doesn't.** `appPullImage` is wired
into the dispatcher but delivers no image on this firmware, and it doesn't
validate the index — so a caller cannot even tell a valid index from a bogus
one. **Wi-Fi sync remains the only route to a full-resolution photo.** Do not
build a BLE image-pull path on this opcode without new evidence.

Not ruled out: an unmet precondition (camera mode, or a required preceding
command) could gate the data path. Nothing in the vendor SDK suggests one, and
the vendor's own app has zero callers for `0x36`, so this was not pursued.

To probe the next unknown opcode, reproduce the method:

1. **Arm inbound logging first**, for a short window around the write. A probe
   harness that logs every inbound frame permanently puts work on the
   per-packet path, which is the one place this protocol cannot afford it — so
   arm it, probe, disarm.
2. Send the opcode with `encode_command` (or `glasses_encode` for a byte
   with no `AppCommand` variant), **varying the payload**. A reply that tracks
   the payload is an echo, not a status.
3. **Always run a bogus-opcode control** (`0xEE`/`0xDE`). Without it you
   cannot tell "firmware recognises this" from "firmware echoes everything",
   and that distinction is the whole result above.

### 13b-1. Why the first probe read "dead" — methodology note

`0x36` was sent to real hardware on 2026-07-28 as `AB 55 00 03 36 00 39`
(twice). The syslog showed the two TX lines and no reply.

**That result is not evidence.** Inbound frames were only being logged behind a
diagnostic flag that was not set, and an unrecognised reply command fell through
the client's dispatch switch silently — so "no log line" could equally mean the
device answered and nobody printed it. Do not record `0x36` as dead the way
`0x01` (LED, §3a) is: the LED verdict rests on the APK teardown, not on log
silence.

Arming inbound logging fixed this, and the very first re-run showed a reply that
had been arriving all along. **The lesson generalises: on this wire, absence of a
log line is not absence of a frame.** Before recording any opcode as dead,
confirm inbound logging is armed, and run a bogus-opcode control so you know
what silence and acknowledgement each look like on the device in front of you.

Note the contrast with `0x01` (LED, §3a), which *is* dead: that verdict rests
on the APK teardown, not on log silence.

## 14. Still open

* Gesture slot ↔ gesture mapping (§5) — change one binding in the vendor app.
* `0x95` and `0x71` bit positions (§7) — need a unit with different flags, or
  a voice feature switched off.
* `0x62`, `0x57`, `0x60`, `0x63`, `0x65`, `0x14`, `0x49`, `0x51`, `0x52`,
  `0x96` — defined but never observed on the wire.

---

## 15. Where each part lives in the crate

| spec | module | the types and functions |
|---|---|---|
| §2 GATT | `gatt` | `SERVICE`, `WRITE`, `CONTROL_NOTIFY`, `FILE_NOTIFY`, `WRITE_WITH_RESPONSE`, `PRESENT_BUT_UNUSED`, `is_drivable` |
| §2 frame envelope | `frame` | `encode`, `encode_command`, `encode_device`, `decode_app`, `decode_device`, `checksum`, `Frame`, `DecodeError` |
| §2 streaming demuxer | `frame` | `Deframer`, `drain`, `Residual`, `is_partial_frame_prefix`, `MAX_LENGTH_FIELD` |
| §3 command table | `opcodes` | `AppCommand`, `DeviceUpload`, `Evidence`, `VENDOR_NAMED_UNIMPLEMENTED` |
| §3 app → device builders | `commands` | one function per row — `take_photo`, `set_volume`, `send_phone_time`, `get_switch_states`, … |
| §3a `0x01` LED | `commands`, `opcodes` | `LedLevel`, `set_led` — the encoder emits it; §3a says why nothing should |
| §4 `0x45` action sync | `parser` | `ActionSync`, `DeviceAction`, `DeviceEvent::ActionSync` — **ten** flags |
| §5 `0x48` switch burst | `parser` | `SwitchStates`, `SwitchReport`, `GestureSlot`, `GestureAction`, `SWITCH_STATE_BURST_ORDER` |
| §6 voice | `voice` | `VoiceStream`, `VoicePacket`, `VoiceEvent`, `EndCause`, `OpusToc`, `OpusMode`, `OpusBandwidth` |
| §7 `0x95` / `0x71` | `parser` | `Capabilities`, `VoiceFeatures`, `DeviceEvent::Capabilities` |
| §8 Wi-Fi opcodes | `commands`, `parser` | `open_wifi`, `WifiService`, `WifiCredentials` (SSID from `0x25`, fixed passphrase) |
| §11 `52 58` file stream | `reassembly` | `FileReassembler`, `FileEvent`, `FileKind`, `FileOpcode`, `decode_file_frame`, `encode_file_frame`, `TransferProgress` |
| everything inbound | `parser` | `Parser::push` → `Vec<DeviceEvent>`; `parse` for a single frame |
| any language | `ffi` (feature `uniffi`) | `GlassesParser`, `GlassesDeframer`, `GlassesFileReassembler`, `GlassesVoiceStream`, `GlassesSwitchStates`, `glasses_*` free functions |

### Not in this crate, by design

This is a sans-IO byte layer. It makes decisions and does arithmetic; it opens
no socket, holds no radio and reads no clock. So these documented parts of the
protocol are deliberately absent, and each is the client's:

* **The SoftAP join** (§8). Picking up the `0x25` SSID and joining
  `192.168.169.1` is `NEHotspotConfiguration` / `WifiNetworkSpecifier` work.
* **The JSON file API** (§9). `/app/getfilelist`, thumbnail and full-file
  download are plain HTTP against the glasses' AP — a REST client, not a codec.
* **RTSP and H.264** (§10). The RTSP message layer is pure enough to port, but
  the socket, the RTP jitter buffer and the decoder are not.
* **Opus decode** (§6). This crate hands you the packet and its geometry —
  frame count, sample rate, bandwidth, duration in µs — parsed from the TOC
  byte. Turning those bytes into PCM is a platform codec (CoreAudio,
  `MediaCodec`, libopus).
* **Every timing duration.** The ~2 s wait after an SSID arrives before joining
  the AP, the ~1.2 s voice-idle gap, the 30 s open-mic cap, the poll interval,
  the reconnect ladder. This crate can NAME the moment a timer expires
  (`EndCause::IdleTimeout`) and cannot measure one.
* **The vendor cloud** (§12), which the glasses never talk to at all.
