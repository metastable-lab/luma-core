//! The `AB 55` / `AC 55` control-packet envelope: encode, decode, and the streaming deframer.
//!
//! Port of the reference iOS client's packet codec (488 lines of Swift) cross-checked against
//! its Android counterpart (236 lines of Kotlin). The two agreed on the envelope
//! and disagreed only on the resync guard — see [`MAX_LENGTH_FIELD`]. Every byte below was
//! then re-verified against the packet captures; the golden vectors in the tests are cut from those
//! captures, not written by hand.
//!
//! ```text
//!   app → device   AB 55 | len (2, BIG-endian) | cmd (1) | data (N) | crc (1)
//!   device → app   AC 55 | len (2, BIG-endian) | cmd (1) | data (N) | crc (1)
//!
//!   len = 1 (cmd) + N (data) + 1 (crc)          so a whole frame is 4 + len bytes
//!   crc = (cmd + Σ data) & 0xFF                 a checksum, NOT a polynomial CRC
//! ```
//!
//! Two properties of that layout are worth stating because they are what make the decoder
//! safe: `len` counts the cmd and the crc but NOT the four header bytes, and the checksum
//! covers the cmd and the data but NOT the header or the length. Getting either boundary
//! wrong produces a decoder that works on `0x00`-payload frames and fails on everything else,
//! which is the shape of bug that survives a smoke test.
//!
//! ## What the captures actually show about fragmentation
//!
//! Both reference implementations implement a streaming demuxer, and the iOS one carries a comment saying the
//! firmware "both fragments one frame across notifications and coalesces several
//! per-notification". **That is not what the captures show.** All eight the packet captures traces
//! were scanned for it: across **15,085** `AC 55` frames, every single frame arrived as
//! exactly one whole ATT value — zero coalesced, zero split. The claim is INFERRED from a
//! client-side symptom (dropped `0x46` voice chunks that the buffering fixed), and the likely
//! real cause is the `AA15` demux, where `52 58` file bytes and `AC 55` frames interleave on
//! one characteristic.
//!
//! The streaming deframer stays anyway, for two reasons that do not depend on the claim being
//! true: it is strictly more general than one-frame-per-notification and costs nothing on the
//! observed traffic, and an MTU or firmware we have not captured could split a 46-byte voice
//! frame at any time. What changes is the honesty of the comment — nobody should cite
//! fragmentation as device-confirmed on the strength of this file.
//!
//! ## No outbound chunking
//!
//! There is no chunking rule on the app→device side, and this is a positive finding rather
//! than an omission: the longest command any client can build is
//! [`AppCommand::SendPhoneTime`](crate::AppCommand::SendPhoneTime) at 12 bytes, comfortably
//! inside even the 23-byte default ATT MTU, and every capture writes whole frames to `AA13`
//! with Write **Request**. Contrast the band vendors, where `ute::frame::chunks` exists
//! because a 20-byte GATT write is the hard limit. A `chunks()` here would be dead code that
//! implied a rule the protocol does not have.

use crate::opcodes::AppCommand;

/// App → device frame magic.
pub const APP_HEADER: [u8; 2] = [0xAB, 0x55];

/// Device → app frame magic.
pub const DEVICE_HEADER: [u8; 2] = [0xAC, 0x55];

/// Magic (2) + length field (2). The length field does NOT count these.
pub const FRAME_OVERHEAD: usize = 4;

/// The smallest legal `len`: one cmd byte plus one crc byte, i.e. a frame with no data at
/// all. Anything below this is corruption, not a short frame.
pub const MIN_LENGTH_FIELD: usize = 2;

/// Largest `len` the deframer will wait on before declaring the length field corrupt.
///
/// This is the one place the two reference implementations disagreed. iOS caps at 4096 and resyncs; Kotlin only
/// rejects `len < 2` and otherwise waits. The iOS behaviour is correct and the divergence is a
/// live bug in the reference Android implementation: a large-but-wrong length falls through to "partial frame —
/// wait" and, because the buffer still begins with a valid magic, looks like a legitimate
/// partial forever — wedging the sparse `AA14` control channel until reconnect, blocked on
/// bytes that will never arrive.
///
/// The value is the reassembly-buffer cap the iOS parser already enforced, and no real frame
/// comes close: the largest ever captured is `len = 0x2A` (42), a voice frame.
///
/// The resync itself is deliberately *local* — [`drain`] drops the two magic bytes and stops,
/// rather than scanning forward for the next `AC 55`. On `AA15`, where raw JPEG bytes share a
/// characteristic with control frames, a forward scan would manufacture a "frame" from image
/// data whenever the magic occurred by chance. See the `Residual::Foreign` path.
pub const MAX_LENGTH_FIELD: usize = 4096;

/// One decoded control frame. `cmd` stays a raw byte on purpose.
///
/// Classification is the router's job, not the codec's: `0x45` and `0x69` are legal in both
/// directions, and most setters come back as verbatim echoes carrying an
/// [`AppCommand`](crate::AppCommand) byte on the *inbound* channel. A codec that returned an
/// enum would have to guess, and guessing is what put a decode branch in the shipped app that
/// no device ever exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub cmd: u8,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn new(cmd: u8, data: impl Into<Vec<u8>>) -> Self {
        Frame {
            cmd,
            data: data.into(),
        }
    }
}

/// Why a byte sequence is not a frame.
///
/// `Truncated` and `BadLength` are deliberately distinct even though both mean "cannot decode
/// this yet": the deframer WAITS on the first and RESYNCS on the second, and collapsing them
/// is exactly how the reference Android implementation came to wedge its control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer than [`FRAME_OVERHEAD`] + [`MIN_LENGTH_FIELD`] bytes — not even a header and an
    /// empty body.
    TooShort,
    /// The first two bytes are not the expected magic.
    BadHeader,
    /// The length field is unusable: below [`MIN_LENGTH_FIELD`] or above
    /// [`MAX_LENGTH_FIELD`]. Never satisfiable by waiting.
    BadLength,
    /// The length field is plausible but the buffer is shorter than `4 + len`. Waiting for
    /// more bytes may fix this.
    Truncated,
    /// Header and length are fine; `(cmd + Σ data) & 0xFF` does not match the trailing byte.
    ChecksumMismatch,
}

/// `(cmd + Σ data) & 0xFF`.
///
/// A plain 8-bit additive checksum over the cmd and the data — the header and the length
/// field are outside it. Both reference implementations compute it identically and every captured frame agrees;
/// the golden-vector tests check it against real bytes rather than against either reference implementation.
pub fn checksum(cmd: u8, data: &[u8]) -> u8 {
    data.iter().fold(cmd, |acc, b| acc.wrapping_add(*b))
}

/// Build an app → device frame with a raw opcode byte.
///
/// Raw rather than typed so an unrecognised opcode can be PROBED without first being added to
/// [`AppCommand`] — that is how `0x36` was characterised, and the probe methodology in
/// `PROTOCOL.md` §13b depends on being able to vary the payload of a command
/// the enum does not know.
///
/// **The empty-payload filler is applied here.** Protocol §2.1.1 requires a command with no
/// payload to carry a single `0x00` ("没有填0x00"), and every capture obeys it: the battery,
/// version and project reads all go out as `ab 55 00 03 <cmd> 00 <crc>`. Truly-empty frames
/// are tolerated by the firmware in front of us but are a spec violation an update could start
/// enforcing, so `encode(cmd, &[])` emits the filler rather than a 2-byte body.
pub fn encode(cmd: u8, data: &[u8]) -> Vec<u8> {
    encode_framed(APP_HEADER, cmd, data)
}

/// [`encode`] with a typed opcode. The only difference is that the call site is readable.
pub fn encode_command(cmd: AppCommand, data: &[u8]) -> Vec<u8> {
    encode(cmd.code(), data)
}

/// Build a device → app frame.
///
/// This crate never plays device on a radio; this exists so tests and fixtures can construct
/// inbound bytes, and so a golden vector can be checked by re-encoding it rather than only by
/// decoding it. Round-tripping a capture through this is what proves the length and checksum
/// boundaries are right in BOTH directions.
pub fn encode_device(cmd: u8, data: &[u8]) -> Vec<u8> {
    encode_framed(DEVICE_HEADER, cmd, data)
}

fn encode_framed(header: [u8; 2], cmd: u8, data: &[u8]) -> Vec<u8> {
    const FILLER: [u8; 1] = [0x00];
    let data = if data.is_empty() { &FILLER[..] } else { data };
    let len = 1 + data.len() + 1; // cmd + data + checksum
    let mut out = Vec::with_capacity(FRAME_OVERHEAD + len);
    out.extend_from_slice(&header);
    out.push((len >> 8) as u8);
    out.push((len & 0xFF) as u8);
    out.push(cmd);
    out.extend_from_slice(data);
    out.push(checksum(cmd, data));
    out
}

/// Decode exactly one device → app frame from the front of `bytes`.
///
/// Trailing bytes are IGNORED, not rejected — a caller holding a coalesced buffer should use
/// [`Deframer`] instead of trimming by hand.
pub fn decode_device(bytes: &[u8]) -> Result<Frame, DecodeError> {
    decode_with(DEVICE_HEADER, bytes)
}

/// Decode exactly one app → device frame. Used to re-read our own output in tests and to
/// parse a capture's outbound side; nothing in the shipped path receives `AB 55`.
pub fn decode_app(bytes: &[u8]) -> Result<Frame, DecodeError> {
    decode_with(APP_HEADER, bytes)
}

fn decode_with(header: [u8; 2], bytes: &[u8]) -> Result<Frame, DecodeError> {
    if bytes.len() < FRAME_OVERHEAD + MIN_LENGTH_FIELD {
        return Err(DecodeError::TooShort);
    }
    if bytes[0] != header[0] || bytes[1] != header[1] {
        return Err(DecodeError::BadHeader);
    }
    let len = ((bytes[2] as usize) << 8) | bytes[3] as usize;
    if !(MIN_LENGTH_FIELD..=MAX_LENGTH_FIELD).contains(&len) {
        return Err(DecodeError::BadLength);
    }
    if bytes.len() < FRAME_OVERHEAD + len {
        return Err(DecodeError::Truncated);
    }
    let cmd = bytes[4];
    let data = &bytes[5..FRAME_OVERHEAD + len - 1];
    let stated = bytes[FRAME_OVERHEAD + len - 1];
    if stated != checksum(cmd, data) {
        return Err(DecodeError::ChecksumMismatch);
    }
    Ok(Frame {
        cmd,
        data: data.to_vec(),
    })
}

/// Could `buf` still grow into a valid frame for `header`?
///
/// The question the deframer asks about whatever it could not consume. `AA14` carries nothing
/// but control frames, so a residual that fails this is junk; `AA15` interleaves `52 58` file
/// frames, so a residual that fails this is file bytes the caller must route on. Feeding a
/// partial frame prefix into a file reassembler corrupts both the JPEG (shifted offsets) and
/// the split control frame, which is a bug the reference iOS implementation fixed the hard way.
pub fn is_partial_frame_prefix(header: [u8; 2], buf: &[u8]) -> bool {
    match buf.first() {
        None => false,
        Some(b) if *b != header[0] => false,
        _ => buf.len() < 2 || buf[1] == header[1],
    }
}

/// What the deframer could not turn into frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Residual {
    /// Every byte was consumed.
    Empty,
    /// A prefix that could still become a frame; it stays buffered for the next chunk. The
    /// count is how many bytes are held.
    PartialFrame(usize),
    /// Bytes that cannot begin a frame. Handed back rather than dropped because on `AA15`
    /// these are the `52 58` file stream, and dropping them loses image data.
    Foreign(Vec<u8>),
    /// A partial frame grew past [`FRAME_OVERHEAD`] + [`MAX_LENGTH_FIELD`] without completing,
    /// so it was dropped rather than allowed to absorb the channel forever. The count is what
    /// was discarded. The threshold is the largest number of bytes a legal frame occupies, NOT
    /// the largest legal length field — a maximum-length frame split across notifications must
    /// reassemble, not be diagnosed as an overflow.
    Overflowed(usize),
}

/// Streaming frame demuxer for one characteristic.
///
/// Feed it whatever a notification delivered; it returns every frame that completed. A frame
/// with a bad checksum is CONSUMED AND DROPPED (the length told us where it ended, so the
/// stream stays aligned); a frame with an impossible length causes a resync past the two
/// magic bytes rather than an indefinite wait.
///
/// Sans-IO: this owns bytes and nothing else. It has no timer, so "the stream went idle" is
/// not a question it can answer — that belongs to the shell, by design. This crate may name
/// a pause; it may not measure one.
#[derive(Debug, Clone)]
pub struct Deframer {
    header: [u8; 2],
    buffer: Vec<u8>,
}

impl Deframer {
    /// For the device → app channels (`AA14` control, `AA15` file/voice).
    pub fn device() -> Self {
        Deframer {
            header: DEVICE_HEADER,
            buffer: Vec::new(),
        }
    }

    /// For reading back our own outbound stream — tests and capture analysis.
    pub fn app() -> Self {
        Deframer {
            header: APP_HEADER,
            buffer: Vec::new(),
        }
    }

    /// Bytes currently held, unparsed.
    pub fn buffered(&self) -> &[u8] {
        &self.buffer
    }

    /// Forget everything buffered. The shell calls this on disconnect, where a half-frame
    /// from the previous link would otherwise prefix the next one.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    /// Feed one notification's bytes; returns the frames it completed.
    ///
    /// The residual policy is applied here, so anything left in [`Self::buffered`] afterwards
    /// is a genuine partial frame. Use [`Self::push_detailed`] when the caller needs the
    /// foreign bytes back (the `AA15` demux does).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.push_detailed(chunk).0
    }

    /// [`Self::push`], plus what happened to the bytes that did not become frames.
    pub fn push_detailed(&mut self, chunk: &[u8]) -> (Vec<Frame>, Residual) {
        self.buffer.extend_from_slice(chunk);
        let frames = drain(self.header, &mut self.buffer);
        let residual = if self.buffer.is_empty() {
            Residual::Empty
        } else if !is_partial_frame_prefix(self.header, &self.buffer) {
            // Cannot start a frame, so it is not ours. `drain` stopped here precisely
            // because it could not consume it, so handing it back terminates.
            Residual::Foreign(std::mem::take(&mut self.buffer))
        // The cap is on the BYTES ON THE WIRE, not on the length field: a frame whose `len` is
        // the legal maximum occupies `FRAME_OVERHEAD + MAX_LENGTH_FIELD` bytes. Comparing
        // against `MAX_LENGTH_FIELD` alone declared the last four bytes of a maximum-length
        // frame an overflow, dropped the frame mid-flight, and left the tail to be handed back
        // as `Foreign` — on `AA15` that is image data, on `AA13` voice PCM.
        //
        // With `drain`'s own arithmetic this is now a backstop rather than a live branch:
        // `drain` only leaves a partial frame buffered when its `len` is legal and fewer than
        // `FRAME_OVERHEAD + len` bytes have arrived, so a genuine partial can never exceed
        // `FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1`. It stays because the invariant is worth
        // asserting at the boundary rather than inferring from a function above it.
        } else if self.buffer.len() > FRAME_OVERHEAD + MAX_LENGTH_FIELD {
            let n = self.buffer.len();
            self.buffer.clear();
            Residual::Overflowed(n)
        } else {
            Residual::PartialFrame(self.buffer.len())
        };
        (frames, residual)
    }
}

/// Decode as many complete frames as `buffer` holds, consuming them in place.
///
/// The free function behind [`Deframer::push`], exposed because it is the whole contract in
/// twenty lines and a caller with its own buffer should not have to adopt the struct. The
/// remainder is left alone: either a trailing partial frame, or bytes that are not ours.
///
/// A checksum-bad frame is consumed and dropped rather than resynced past — the length field
/// already told us where it ends, and treating it as garbage would resynchronise into the
/// middle of the NEXT frame and cascade.
pub fn drain(header: [u8; 2], buffer: &mut Vec<u8>) -> Vec<Frame> {
    let mut out = Vec::new();
    loop {
        if buffer.is_empty() || buffer[0] != header[0] {
            break; // not ours, or nothing left
        }
        if buffer.len() < 2 {
            break; // partial magic — wait
        }
        if buffer[1] != header[1] {
            break; // not ours
        }
        if buffer.len() < FRAME_OVERHEAD {
            break; // length field not fully arrived — wait
        }
        // Validate the length as soon as all four header bytes are in hand, not once a cmd
        // byte has also arrived (the reference iOS implementation waits for five). A corrupt length is then
        // caught one notification earlier, which on the sparse AA14 channel can be seconds.
        let len = ((buffer[2] as usize) << 8) | buffer[3] as usize;
        if !(MIN_LENGTH_FIELD..=MAX_LENGTH_FIELD).contains(&len) {
            // Corrupt or unsatisfiable length. Drop the magic so the remainder can never be
            // mistaken for a partial frame, then let the loop re-test: waiting instead would
            // block the channel on bytes that never arrive. Note this does NOT hunt for the
            // next magic — see MAX_LENGTH_FIELD for why a forward scan is wrong here.
            buffer.drain(..2);
            continue;
        }
        let total = FRAME_OVERHEAD + len;
        if buffer.len() < total {
            break; // partial frame — wait for the next notification
        }
        if let Ok(frame) = decode_with(header, &buffer[..total]) {
            out.push(frame);
        }
        buffer.drain(..total);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcodes::{DeviceUpload, GestureSlot};

    fn hex(s: &str) -> Vec<u8> {
        assert!(
            s.len().is_multiple_of(2),
            "hex literal must be byte-aligned"
        );
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Frames lifted verbatim out of the packet captures — `(hex, cmd, data, provenance)`.
    ///
    /// This is the table the whole module is verified against. Each entry was read off the
    /// wire with `tshark -r <capture>.pklg -Y btatt.value`, so a decode that agrees with both
    /// Swift and Kotlin but disagrees with one of these is wrong, not merely different.
    const GOLDEN_DEVICE: &[(&str, u8, &[u8], &str)] = &[
        (
            "ac550006950000040ba4",
            0x95,
            &[0x00, 0x00, 0x04, 0x0B],
            "capability push, unsolicited ~30ms after link-up; identical on 6 units",
        ),
        (
            "ac550009550104080103010269",
            0x55,
            &[0x01, 0x04, 0x08, 0x01, 0x03, 0x01, 0x02],
            "versions: bt V1.4.8 / isp V1.3.1 / hw V2",
        ),
        (
            "ac55000a645431000030333033af",
            0x64,
            &[0x54, 0x31, 0x00, 0x00, 0x30, 0x33, 0x30, 0x33],
            "project name \"T1\" + customer \"0303\", 4 ASCII bytes each",
        ),
        (
            "ac55000f2544482d5477492d34363431453467",
            0x25,
            b"DH-TwI-4641E4",
            "SoftAP SSID, ~2.5s after 0x39/0x67",
        ),
        (
            "ac55000c450000000000000000000045",
            0x45,
            &[0; 10],
            "action sync — TEN bytes, not the PDF's nine",
        ),
        (
            "ac550005173a300182",
            0x17,
            &[0x3A, 0x30, 0x01],
            "battery 100%: ASCII digits overflow '9'+1 = 0x3A, charging",
        ),
        (
            "ac5500051734390185",
            0x17,
            &[0x34, 0x39, 0x01],
            "battery 49% — the same field, in range",
        ),
        (
            "ac55000453013185",
            0x53,
            &[0x01, 0x31],
            "battery push: charging + RAW 49, not ASCII like 0x17",
        ),
        (
            "ac55000402003c3e",
            0x02,
            &[0x00, 0x3C],
            "record duration 60s — 2-byte BE, the one non-ASCII burst value",
        ),
        (
            "ac550005690607067c",
            0x69,
            &[0x06, 0x07, 0x06],
            "volumes [system, media, call] after three 0x70 writes of 06/07/06",
        ),
        ("ac550003970198", 0x97, &[0x01], "wake-word capture started"),
        (
            "ac550003670168",
            0x67,
            &[0x01],
            "openWiFi-live ack — note the echo is 01, NOT the 0x30 we sent",
        ),
        (
            "ac55000442000f51",
            0x42,
            &[0x00, 0x0F],
            "thumbnail count 15, 2-byte BE",
        ),
    ];

    /// The `0x48` switch-state burst as captured, in order — ten frames, no `0x48` among
    /// them. Kept separate from `GOLDEN_DEVICE` because its value is the SEQUENCE.
    const GOLDEN_BURST: &[(&str, u8, &[u8])] = &[
        ("ac550003013132", 0x01, b"1"),
        ("ac55000402003c3e", 0x02, &[0x00, 0x3C]),
        ("ac550003043135", 0x04, b"1"),
        ("ac550003063137", 0x06, b"1"),
        ("ac550003073037", 0x07, b"0"),
        ("ac550003083139", 0x08, b"1"),
        ("ac55000309323b", 0x09, b"2"),
        ("ac550003103444", 0x10, b"4"),
        ("ac550003113344", 0x11, b"3"),
        ("ac550003613091", 0x61, b"0"),
    ];

    /// Real outbound frames from the same captures.
    const GOLDEN_APP: &[(&str, u8, &[u8], &str)] = &[
        (
            "ab550008591a071b122812e1",
            0x59,
            &[0x1A, 0x07, 0x1B, 0x12, 0x28, 0x12],
            "phone time 2026-07-27 18:40:18 — plain hex, NOT BCD",
        ),
        (
            "ab550003640064",
            0x64,
            &[0x00],
            "project-name read, 0x00 filler",
        ),
        ("ab550003170017", 0x17, &[0x00], "battery read, 0x00 filler"),
        (
            "ab550003673097",
            0x67,
            &[0x30],
            "openWiFi LIVE, AP mode (GlassX)",
        ),
        ("ab550003393069", 0x39, &[0x30], "openWiFi FILES, AP mode"),
        (
            "ab55000470010778",
            0x70,
            &[0x01, 0x07],
            "set media volume to 7",
        ),
        (
            "ab55000444300175",
            0x44,
            &[0x30, 0x01],
            "ISP off without clearing the count",
        ),
        (
            "ab5500040200b4b6",
            0x02,
            &[0x00, 0xB4],
            "record duration 180s",
        ),
    ];

    /// The load-bearing test: every golden capture frame decodes to the stated cmd/data AND
    /// re-encodes to the original bytes. Round-tripping in both directions is what pins the
    /// length and checksum BOUNDARIES — a decoder that includes the header in the checksum,
    /// or counts the header in `len`, passes a decode-only test on `00`-payload frames and
    /// fails here on the first multi-byte one.
    #[test]
    fn every_captured_device_frame_decodes_and_re_encodes_byte_for_byte() {
        for (h, cmd, data, why) in GOLDEN_DEVICE {
            let bytes = hex(h);
            let got = decode_device(&bytes).unwrap_or_else(|e| panic!("{h} ({why}): {e:?}"));
            assert_eq!(got.cmd, *cmd, "{why}");
            assert_eq!(got.data, *data, "{why}");
            assert_eq!(encode_device(got.cmd, &got.data), bytes, "{why}");
        }
    }

    #[test]
    fn every_captured_app_frame_decodes_and_re_encodes_byte_for_byte() {
        for (h, cmd, data, why) in GOLDEN_APP {
            let bytes = hex(h);
            let got = decode_app(&bytes).unwrap_or_else(|e| panic!("{h} ({why}): {e:?}"));
            assert_eq!(got.cmd, *cmd, "{why}");
            assert_eq!(got.data, *data, "{why}");
            assert_eq!(encode(got.cmd, &got.data), bytes, "{why}");
        }
    }

    /// The gesture frames Android has never had, decoded from the bytes six devices sent.
    /// This is the concrete payoff of the port: the burst arrives, the slots are in it, and
    /// one codec now reads them for both platforms.
    #[test]
    fn the_captured_switch_state_burst_carries_all_five_gesture_slots() {
        let mut deframer = Deframer::device();
        let mut all = Vec::new();
        for (h, cmd, data) in GOLDEN_BURST {
            let bytes = hex(h);
            let f = decode_device(&bytes).expect("burst frame decodes");
            assert_eq!((f.cmd, f.data.as_slice()), (*cmd, *data), "{h}");
            all.extend_from_slice(&bytes);
        }
        // …and the whole burst, concatenated as one blob, demuxes back into ten frames.
        let frames = deframer.push(&all);
        assert_eq!(frames.len(), 10);
        assert!(deframer.buffered().is_empty());

        let slots: Vec<u8> = frames
            .iter()
            .filter(|f| GestureSlot::from_code(f.cmd).is_some())
            .map(|f| f.data[0])
            .collect();
        assert_eq!(
            slots,
            GestureSlot::CAPTURED_DEFAULTS,
            "the '0 1 2 4 3' default"
        );
        // No frame in the burst is keyed 0x48 — the request opcode never comes back.
        assert!(!frames.iter().any(|f| f.cmd == 0x48));
    }

    /// A real 0x46 voice frame: `len = 0x2A` = 42 = 1 cmd + 40 data + 1 checksum, arriving as
    /// a 46-byte notification. the captures hold 15,085 of these, and they are the reason the
    /// hot path must not allocate per byte.
    #[test]
    fn a_captured_voice_frame_has_forty_payload_bytes() {
        let bytes = hex(
            "ac55002a464b411e07c972430d80b6000000000000000000000000000000000000\
             000000000000000000000000b8",
        );
        assert_eq!(bytes.len(), FRAME_OVERHEAD + 0x2A);
        let f = decode_device(&bytes).expect("voice frame decodes");
        assert_eq!(f.cmd, DeviceUpload::VoiceData.code());
        assert_eq!(f.data.len(), 40);
        assert_eq!(encode_device(f.cmd, &f.data), bytes);
    }

    /// `len` excludes the header; the checksum excludes the header AND the length. Asserted
    /// on a capture frame with a multi-byte payload, where both boundaries are visible.
    #[test]
    fn length_and_checksum_boundaries_are_where_the_captures_put_them() {
        let bytes = hex("ac55000a645431000030333033af");
        let len = ((bytes[2] as usize) << 8) | bytes[3] as usize;
        assert_eq!(len, 10, "cmd + 8 data + checksum");
        assert_eq!(
            bytes.len(),
            FRAME_OVERHEAD + len,
            "header is NOT counted in len"
        );
        // Checksum covers cmd + data only.
        assert_eq!(checksum(0x64, &bytes[5..13]), 0xAF);
        assert_eq!(*bytes.last().unwrap(), 0xAF);
        // Including the header would give a different byte — proof the boundary matters.
        assert_ne!(checksum(0xAC, &bytes[1..13]), 0xAF);
    }

    /// The `0x00` filler for an empty payload, confirmed against three captured reads that
    /// all carry it (`ab 55 00 03 <cmd> 00 <crc>`). Emitting a truly-empty body is a §2.1.1
    /// violation this firmware tolerates and a future one may not.
    #[test]
    fn an_empty_payload_becomes_the_protocol_mandated_filler_byte() {
        assert_eq!(encode(0x17, &[]), hex("ab550003170017"));
        assert_eq!(encode(0x17, &[0x00]), hex("ab550003170017"));
        assert_eq!(
            encode_command(AppCommand::GetBattery, &[]),
            hex("ab550003170017")
        );
        // The filler is a real data byte, so it survives the round trip.
        assert_eq!(decode_app(&encode(0x17, &[])).unwrap().data, vec![0x00]);
    }

    /// Coalesced and fragmented delivery. Neither pattern appears in any of the eight
    /// captures (see the module docs), so this is a robustness test rather than a fixture —
    /// and it is labelled that way so nobody later cites it as device evidence.
    #[test]
    fn frames_survive_being_coalesced_and_split_arbitrarily() {
        let a = hex("ac550003970198");
        let b = hex("ac550006950000040ba4");
        let c = hex("ac55000442000f51");
        let stream: Vec<u8> = [a.clone(), b.clone(), c.clone()].concat();

        // All three in one notification.
        let mut d = Deframer::device();
        let frames = d.push(&stream);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].cmd, 0x97);
        assert_eq!(frames[2].data, vec![0x00, 0x0F]);

        // The same stream split at every possible boundary yields the same three frames.
        for cut in 1..stream.len() {
            let mut d = Deframer::device();
            let mut got = d.push(&stream[..cut]);
            got.extend(d.push(&stream[cut..]));
            assert_eq!(got.len(), 3, "split at {cut}");
            assert_eq!(got[1].data, vec![0x00, 0x00, 0x04, 0x0B], "split at {cut}");
            assert!(d.buffered().is_empty(), "split at {cut}");
        }
    }

    /// The reference Android implementation's live bug, pinned. A length that is plausible-looking but
    /// unsatisfiable must NOT be waited on: the bytes have to leave the buffer, or the sparse
    /// `AA14` control channel wedges until reconnect and the user sees "the glasses stopped
    /// responding".
    ///
    /// Note what the resync deliberately does NOT do: it drops the two magic bytes and stops,
    /// rather than scanning forward for the next `AC 55`. The frame after the corruption is
    /// therefore surrendered along with it. That is the right trade on this wire — `AA15`
    /// interleaves raw JPEG bytes with control frames, and a forward scan would rip a
    /// "frame" out of the middle of image data whenever `AC 55` occurred by chance. Losing
    /// one frame after a corruption beats fabricating one out of a photo.
    #[test]
    fn an_oversized_length_releases_the_buffer_instead_of_waiting_forever() {
        let mut stream = vec![0xAC, 0x55, 0xFF, 0xFF]; // len = 65535 > MAX_LENGTH_FIELD
        stream.extend_from_slice(&hex("ac550003970198"));

        let mut d = Deframer::device();
        let (frames, residual) = d.push_detailed(&stream);
        assert!(frames.is_empty());
        // The magic is gone, so the remainder can no longer masquerade as a partial frame —
        // it comes back as foreign bytes the caller disposes of, and the buffer is empty.
        assert!(
            matches!(&residual, Residual::Foreign(b) if b.starts_with(&[0xFF, 0xFF])),
            "{residual:?}"
        );
        assert!(
            d.buffered().is_empty(),
            "an unsatisfiable length must not stay buffered"
        );
        // The next notification starts clean, so one bad length costs one resync, not a link.
        let frames = d.push(&hex("ac550006950000040ba4"));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].cmd, 0x95);

        assert_eq!(
            decode_device(&[0xAC, 0x55, 0xFF, 0xFF, 0, 0]),
            Err(DecodeError::BadLength)
        );
    }

    /// `len < 2` cannot describe even a cmd and a checksum. Same resync, different cause.
    #[test]
    fn an_impossibly_small_length_also_resyncs() {
        let mut d = Deframer::device();
        let (frames, residual) = d.push_detailed(&[0xAC, 0x55, 0x00, 0x01, 0x00]);
        assert!(frames.is_empty());
        assert!(matches!(residual, Residual::Foreign(_)), "{residual:?}");
        assert!(d.buffered().is_empty());
        assert_eq!(
            decode_device(&[0xAC, 0x55, 0x00, 0x01, 0x00, 0x00]),
            Err(DecodeError::BadLength)
        );
    }

    /// A bad checksum consumes the frame rather than resyncing past it. The length field
    /// already located the end; resyncing would land in the middle of the NEXT frame and turn
    /// one corrupt packet into a cascade.
    #[test]
    fn a_checksum_failure_drops_one_frame_and_keeps_the_stream_aligned() {
        let mut bad = hex("ac550003970198");
        *bad.last_mut().unwrap() = 0x00;
        let mut stream = bad.clone();
        stream.extend_from_slice(&hex("ac550006950000040ba4"));

        assert_eq!(decode_device(&bad), Err(DecodeError::ChecksumMismatch));

        let mut d = Deframer::device();
        let frames = d.push(&stream);
        assert_eq!(frames.len(), 1, "only the good frame survives");
        assert_eq!(frames[0].cmd, 0x95);
        assert!(d.buffered().is_empty());
    }

    /// The `AA15` demux contract. That characteristic interleaves `52 58` file frames with
    /// `AC 55` control frames, so leftover bytes must come BACK to the caller. Dropping them
    /// loses image data; feeding a partial control-frame prefix into the file reassembler
    /// corrupts the JPEG and the split frame both.
    #[test]
    fn foreign_trailing_bytes_are_handed_back_and_partial_prefixes_are_kept() {
        let mut d = Deframer::device();
        let mut stream = hex("ac550003970198");
        stream.extend_from_slice(&[0x52, 0x58, 0x00, 0x07]); // start of a file frame

        let (frames, residual) = d.push_detailed(&stream);
        assert_eq!(frames.len(), 1);
        assert_eq!(residual, Residual::Foreign(vec![0x52, 0x58, 0x00, 0x07]));
        assert!(
            d.buffered().is_empty(),
            "foreign bytes must not stay buffered"
        );

        // A genuine partial control frame is kept instead.
        let (frames, residual) = d.push_detailed(&[0xAC, 0x55, 0x00]);
        assert!(frames.is_empty());
        assert_eq!(residual, Residual::PartialFrame(3));
        assert_eq!(d.buffered(), &[0xAC, 0x55, 0x00]);

        // `AC` alone is ambiguous — it could still become `AC 55`.
        assert!(is_partial_frame_prefix(DEVICE_HEADER, &[0xAC]));
        assert!(is_partial_frame_prefix(DEVICE_HEADER, &[0xAC, 0x55]));
        assert!(!is_partial_frame_prefix(DEVICE_HEADER, &[0xAC, 0x56]));
        assert!(!is_partial_frame_prefix(DEVICE_HEADER, &[0x52, 0x58]));
        assert!(!is_partial_frame_prefix(DEVICE_HEADER, &[]));
    }

    /// The overflow cap is on the BYTES A FRAME OCCUPIES, not on the length field, and the
    /// difference is four bytes of real traffic.
    ///
    /// A frame with the maximum legal `len` (4096) is `FRAME_OVERHEAD + 4096` = 4100 bytes on
    /// the wire, so while it is still arriving the buffer legitimately holds up to 4099. The
    /// cap used to be `MAX_LENGTH_FIELD` itself, which diagnosed those last three bytes as an
    /// overflow: the in-flight frame was dropped and its tail handed back as `Foreign`.
    #[test]
    fn a_maximum_length_frame_still_arriving_is_not_an_overflow() {
        // Claims len = 0x1000 (4096, exactly the cap, so the length itself is legal) and then
        // falls three bytes short of the 4100 the frame needs.
        let mut stream = vec![0xAC, 0x55, 0x10, 0x00];
        stream.extend(vec![0u8; MAX_LENGTH_FIELD - 1]);
        assert_eq!(stream.len(), FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1);

        let mut d = Deframer::device();
        let (frames, residual) = d.push_detailed(&stream);
        assert!(frames.is_empty(), "the frame is genuinely incomplete");
        assert_eq!(
            residual,
            Residual::PartialFrame(FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1),
            "the largest legal in-flight prefix is a wait, never an overflow"
        );
        assert_eq!(d.buffered().len(), FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1);

        // And well inside it, likewise.
        let mut d = Deframer::device();
        let (_, residual) = d.push_detailed(&stream[..MAX_LENGTH_FIELD]);
        assert_eq!(residual, Residual::PartialFrame(MAX_LENGTH_FIELD));
    }

    /// The bug the boundary above protects, end to end: a maximum-length frame delivered in
    /// two notifications must reassemble into one frame with its payload intact.
    #[test]
    fn a_maximum_length_frame_split_across_notifications_reassembles() {
        let payload = vec![0x5A; MAX_LENGTH_FIELD - 2]; // cmd + data + crc == MAX_LENGTH_FIELD
        let bytes = encode_device(0x95, &payload);
        assert_eq!(bytes.len(), FRAME_OVERHEAD + MAX_LENGTH_FIELD);
        assert_eq!(
            ((bytes[2] as usize) << 8) | bytes[3] as usize,
            MAX_LENGTH_FIELD,
            "the length field is at its legal maximum"
        );

        let mut d = Deframer::device();
        // Split so the first piece already exceeds MAX_LENGTH_FIELD — that is exactly where
        // the old cap fired.
        let split = FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1;
        let (frames, residual) = d.push_detailed(&bytes[..split]);
        assert!(frames.is_empty());
        assert_eq!(residual, Residual::PartialFrame(split));

        let (frames, residual) = d.push_detailed(&bytes[split..]);
        assert_eq!(residual, Residual::Empty);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].cmd, 0x95);
        assert_eq!(frames[0].data, payload);
    }

    /// A corrupt length is still resynced past rather than waited on — the other way the
    /// channel could stall. `Overflowed` is now only a backstop behind this: once `drain`
    /// rejects an illegal length locally, a buffered partial can never exceed
    /// `FRAME_OVERHEAD + MAX_LENGTH_FIELD - 1`, which is what the two tests above pin.
    #[test]
    fn an_illegal_length_resyncs_rather_than_waiting() {
        let mut d = Deframer::device();
        // len = 0xFFFF, far past MAX_LENGTH_FIELD: dropping the magic is the resync.
        let (frames, residual) = d.push_detailed(&[0xAC, 0x55, 0xFF, 0xFF, 0x01]);
        assert!(frames.is_empty());
        assert_eq!(residual, Residual::Foreign(vec![0xFF, 0xFF, 0x01]));
    }

    /// The two directions must not accept each other's magic. `AB 55` arriving on a notify
    /// characteristic is not a frame we sent coming home — it is something else entirely, and
    /// decoding it would attribute a device state to our own request.
    #[test]
    fn the_two_headers_are_not_interchangeable() {
        let device = hex("ac550003970198");
        assert_eq!(decode_app(&device), Err(DecodeError::BadHeader));
        let app = hex("ab550003170017");
        assert_eq!(decode_device(&app), Err(DecodeError::BadHeader));
        // And a deframer for one channel leaves the other's bytes alone rather than
        // consuming them.
        let mut d = Deframer::device();
        let (frames, residual) = d.push_detailed(&app);
        assert!(frames.is_empty());
        assert_eq!(residual, Residual::Foreign(app));
    }

    #[test]
    fn short_input_is_too_short_not_a_header_or_checksum_error() {
        assert_eq!(decode_device(&[]), Err(DecodeError::TooShort));
        assert_eq!(
            decode_device(&[0xAC, 0x55, 0x00]),
            Err(DecodeError::TooShort)
        );
        assert_eq!(
            decode_device(&[0xAC, 0x55, 0x00, 0x06, 0x95]),
            Err(DecodeError::TooShort)
        );
        // Header + length present, body short: waiting can fix this, so it is Truncated.
        assert_eq!(
            decode_device(&hex("ac5500069500")),
            Err(DecodeError::Truncated)
        );
    }

    /// The checksum wraps rather than saturating. A payload that sums past 0xFF is common —
    /// the captured SSID frame alone sums to 0x567 — so a saturating fold would reject
    /// almost every real frame.
    #[test]
    fn the_checksum_wraps_at_eight_bits() {
        assert_eq!(checksum(0xFF, &[0x01]), 0x00);
        assert_eq!(checksum(0x25, b"DH-TwI-4641E4"), 0x67);
        assert_eq!(checksum(0x00, &[]), 0x00);
    }

    /// Arbitrary payload lengths round-trip. Not a substitute for the golden vectors — a
    /// self-consistent codec can round-trip perfectly while being wrong on the wire — but it
    /// covers the sizes no capture happens to contain.
    #[test]
    fn encode_decode_round_trips_across_payload_sizes() {
        for n in 1..=64usize {
            let data: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_mul(37)).collect();
            let bytes = encode(0x22, &data);
            assert_eq!(bytes.len(), FRAME_OVERHEAD + 1 + n + 1);
            let f = decode_app(&bytes).expect("round trip");
            assert_eq!(f, Frame::new(0x22, data));
        }
    }

    /// Every command a client can build fits one ATT write, even at the 23-byte default MTU.
    /// This is why there is no `chunks()` in this module, stated as an assertion so that a
    /// future command with a long payload fails here rather than silently truncating.
    #[test]
    fn no_command_needs_chunking_at_the_default_att_mtu() {
        const DEFAULT_ATT_WRITE_PAYLOAD: usize = 20; // MTU 23 − 3 bytes of ATT header
        for (h, ..) in GOLDEN_APP {
            assert!(hex(h).len() <= DEFAULT_ATT_WRITE_PAYLOAD, "{h}");
        }
        // The longest command any client builds is the six-field time push.
        assert_eq!(encode_command(AppCommand::SendPhoneTime, &[0; 6]).len(), 12);
    }
}
