//! The wake-word voice stream: the `0x97` → `0x46`… → `0x99` state machine, and Opus PACKET
//! framing.
//!
//! Port of the reference iOS client's voice stream decoder (72 lines of Swift) and
//! its Android counterpart (66 lines of Kotlin), re-derived against 14,636 `0x46`
//! frames captured in capture `eyevue_1`, capture `eyevue_2`, capture `client_1` and
//! capture `client_2`.
//!
//! ## Where the line is drawn — this module does NOT decode audio
//!
//! Each `0x46` frame's 40-byte payload is one complete **Opus** packet (RFC 6716). Two layers
//! live in that sentence and only the first is here:
//!
//! | | Layer | Where it lives |
//! |---|---|---|
//! | ✅ | Packet framing — the TOC byte, the frame-count byte, the padding fields, how many audio frames the packet holds and how long they are | [`OpusToc`], this module |
//! | ❌ | The SILK range decoder that turns those bytes into PCM | the client — the reference implementations use Apple CoreAudio (no libopus) and Android `MediaCodec` |
//!
//! The split is not arbitrary. Packet framing is bit arithmetic over six header bits and is the
//! same on every platform, so a divergence in it is a bug by definition. Audio decode is a
//! platform codec — CoreAudio on one side, `MediaCodec` on the other — and a Rust codec here
//! would mean shipping a third implementation of something both operating systems already
//! contain. So [`VoiceStream`] hands the shell a [`VoicePacket`] whose `payload` goes straight
//! into the native decoder, plus the geometry the shell needs to size the PCM buffer it will
//! get back.
//!
//! Both reference implementations conflate the two: their `handle()` returns already-decoded PCM, which is why
//! their unit tests cannot assert on audio at all (the iOS suite says so out loud — "the Opus
//! decode step may produce no audio on a Simulator"). Splitting them makes the framing testable
//! without a codec, which is the whole reason it is worth moving.
//!
//! ## What the captures establish
//!
//! Every one of the 14,636 payloads is exactly 40 bytes and decodes as:
//!
//! * **config 9** — SILK-only, **wideband (16 kHz)**, **20 ms** frames. Unanimous; no other
//!   config value appears anywhere.
//! * **mono** — the TOC stereo bit is 0 in every frame.
//! * **exactly one audio frame per packet** — code 0 (517 frames) or code 3 with a frame count
//!   of 1 (14,119 frames). No packet has ever carried 2, 3 or more.
//! * **CBR, padded to a fixed 40 bytes.** The code-3 packets set Opus's own padding flag and
//!   carry a padding-length byte; `3 + data + padding == 40` holds exactly. The trailing zeroes
//!   both reference implementations' hex dumps show are Opus padding, not silence and not truncation.
//!
//! That independently CONFIRMS the "SILK-WB 16 kHz / 20 ms mono" comment both reference implementations carry — it
//! was taken from the vendor PDF's `格式为opus`, and the bytes agree with it.
//!
//! ## The mic never closes on its own
//!
//! `0x99` is declared by both reference implementations and the PDF and is **never sent by this firmware**. Four
//! captures totalling 14,636 voice frames contain zero of them; `client_1` streamed 79.7 s solid
//! and `client_2` 65.1 s, neither with any app-side close. The only thing that stops the stream is
//! the app writing [`InterruptVoice`](crate::AppCommand::InterruptVoice) (`0x56`), which the
//! vendor sends in pairs ~0.5 s apart.
//!
//! Both reference implementations model the close as *state cleanup with no event*: `interrupt()` sets
//! `isCapturing = false` and returns nothing. So on real firmware the utterance never gets an
//! end signal at all, and everything downstream has to infer one. Here every way a capture can
//! end produces [`VoiceEvent::Ended`] carrying an [`EndCause`], and the one cause that would
//! come from the device is marked as the one nobody has observed.
//!
//! **`0x99` is still on the wire — in the other framing.** It is the file-transfer terminator
//! inside a `52 58` frame (see [`crate::reassembly`]). Same byte, different envelope, different
//! meaning; a router keyed on the byte alone will end a voice capture when a photo finishes.
//!
//! ## Sans-IO
//!
//! No clock, so the endpointing this stream needs is not done here (client policy). The vendor's
//! ~1.2 s inter-frame gap, its 24–30 s open-mic windows and clients' 30 s cap are all
//! DURATIONS the shell owns; this module only names the moment they expire, as
//! [`EndCause::IdleTimeout`]. Nor is endpointing attempted from the audio: the firmware streams
//! open-mic and sends `0x46` through silence, so utterance boundaries come from the
//! recogniser's own segmentation, exactly as both reference implementations say.

use crate::frame::Frame;
use crate::opcodes::DeviceUpload;

/// The PCM rate both reference implementations ask their native decoder to produce.
///
/// Not a property of the wire — Opus always decodes at 48 kHz internally and the caller chooses
/// the output rate. This value is the one to choose: the captured packets are wideband
/// ([`OpusBandwidth::Wide`]), whose natural rate is 16 kHz, so asking for 16 kHz costs no
/// resampling and loses nothing.
pub const REQUESTED_SAMPLE_RATE_HZ: u32 = 16_000;

/// Sample depth of the decoded PCM both reference implementations produce.
pub const BITS_PER_SAMPLE: u32 = 16;

/// Decoded channel count. The TOC stereo bit is 0 in every captured frame.
pub const CHANNELS: u32 = 1;

/// Payload bytes in every captured `0x46` frame — 14,636 of them, without exception.
///
/// The device pads to this with Opus's own padding mechanism rather than varying the frame
/// length, so it is a constant of this firmware and not of the codec. Nothing here depends on
/// it; it is recorded so a change is noticeable.
pub const CAPTURED_PACKET_BYTES: usize = 40;

/// Opus operating mode, derived from the TOC config number (RFC 6716 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpusMode {
    /// Configs 0–11. What every captured frame uses.
    Silk,
    /// Configs 12–15.
    Hybrid,
    /// Configs 16–31.
    Celt,
}

/// Opus audio bandwidth, derived from the TOC config number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpusBandwidth {
    Narrow,
    Medium,
    /// Configs 8–11 and 20–23. Config 9 — every captured frame — is here.
    Wide,
    SuperWide,
    Full,
}

impl OpusBandwidth {
    /// The sample rate this bandwidth is naturally decoded at, in Hz.
    ///
    /// Requesting exactly this from the native decoder avoids a resample. For the captured
    /// stream it is [`REQUESTED_SAMPLE_RATE_HZ`], which is where that constant comes from.
    pub fn nominal_rate_hz(self) -> u32 {
        match self {
            OpusBandwidth::Narrow => 8_000,
            OpusBandwidth::Medium => 12_000,
            OpusBandwidth::Wide => 16_000,
            OpusBandwidth::SuperWide => 24_000,
            OpusBandwidth::Full => 48_000,
        }
    }
}

/// The self-describing header of one Opus packet: RFC 6716 §3.1's TOC byte, plus §3.2's
/// frame-count byte when the packet uses code 3.
///
/// ```text
///   TOC   c c c c c s f f      ccccc = config 0..31, s = stereo, ff = frame-count code
///   code 3 adds  v p m m m m m m      v = VBR, p = padding present, mmmmmm = frame count
/// ```
///
/// This is packet FRAMING, not the codec: it says how many audio frames the packet holds and how
/// long each one is, and says nothing about what they contain. The range decoder that turns the
/// remaining bytes into samples stays native — see this module's docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusToc {
    /// TOC bits 3–7, `0..=31`. Selects mode, bandwidth and frame duration together.
    pub config: u8,
    /// TOC bit 2. `false` in every captured frame.
    pub stereo: bool,
    /// TOC bits 0–1: 0 = one frame, 1 = two equal frames, 2 = two frames of different sizes,
    /// 3 = an arbitrary count given by the following byte.
    pub code: u8,
    /// How many audio frames the packet holds. `1` in every captured frame.
    pub frames: u8,
    /// Code 3 only: the frames have individually-coded lengths.
    pub vbr: bool,
    /// Code 3 only: a padding-length field follows and the packet is padded to a fixed size.
    /// Set on 14,119 of the 14,636 captured frames — this is what makes them all 40 bytes.
    pub padding: bool,
}

impl OpusToc {
    /// Parse the header of one Opus packet.
    ///
    /// `None` for an empty packet, and for a code-3 packet whose frame-count byte is missing —
    /// both are malformed rather than merely unusual, and returning a default would invent a
    /// geometry the bytes do not state. Nothing beyond the header is examined, so a packet whose
    /// PAYLOAD is truncated still parses: this cannot and does not validate the audio.
    pub fn parse(packet: &[u8]) -> Option<OpusToc> {
        let toc = *packet.first()?;
        let code = toc & 0b11;
        let (frames, vbr, padding) = match code {
            0 => (1, false, false),
            1 => (2, false, false),
            2 => (2, true, false),
            _ => {
                let count = *packet.get(1)?;
                (
                    count & 0b0011_1111,
                    count & 0b1000_0000 != 0,
                    count & 0b0100_0000 != 0,
                )
            }
        };
        Some(OpusToc {
            config: toc >> 3,
            stereo: toc & 0b100 != 0,
            code,
            frames,
            vbr,
            padding,
        })
    }

    /// SILK, Hybrid or CELT.
    pub fn mode(self) -> OpusMode {
        match self.config {
            0..=11 => OpusMode::Silk,
            12..=15 => OpusMode::Hybrid,
            _ => OpusMode::Celt,
        }
    }

    /// The audio bandwidth, whose [`nominal_rate_hz`](OpusBandwidth::nominal_rate_hz) is the
    /// rate to request from the native decoder.
    pub fn bandwidth(self) -> OpusBandwidth {
        match self.config {
            0..=3 => OpusBandwidth::Narrow,
            4..=7 => OpusBandwidth::Medium,
            8..=11 => OpusBandwidth::Wide,
            12..=13 => OpusBandwidth::SuperWide,
            14..=15 => OpusBandwidth::Full,
            16..=19 => OpusBandwidth::Narrow,
            20..=23 => OpusBandwidth::Wide,
            24..=27 => OpusBandwidth::SuperWide,
            _ => OpusBandwidth::Full,
        }
    }

    /// How much audio ONE frame of this packet encodes, in microseconds.
    ///
    /// This is a decoded property of the bytes in hand — the packet says so — and not a timing
    /// policy of the sort this crate keeps out. Microseconds because the CELT
    /// configs allow 2.5 ms, which is not an integer number of milliseconds.
    pub fn frame_duration_us(self) -> u32 {
        match self.config {
            0..=11 => [10_000, 20_000, 40_000, 60_000][(self.config % 4) as usize],
            12..=15 => [10_000, 20_000][(self.config % 2) as usize],
            _ => [2_500, 5_000, 10_000, 20_000][(self.config % 4) as usize],
        }
    }

    /// Total audio in the packet, in microseconds. `None` for a frame count of zero, which is
    /// malformed — RFC 6716 requires at least one.
    pub fn total_duration_us(self) -> Option<u32> {
        (self.frames > 0).then(|| self.frame_duration_us() * self.frames as u32)
    }

    /// How many samples PER CHANNEL a decoder asked for `rate_hz` will return for this packet.
    ///
    /// What the shell needs to size the buffer it hands the native decoder. Computed in `u64`
    /// because 60 ms at 48 kHz times a 48-frame packet overflows `u32` on the way through.
    pub fn decoded_samples(self, rate_hz: u32) -> Option<usize> {
        let us = self.total_duration_us()? as u64;
        Some((us * rate_hz as u64 / 1_000_000) as usize)
    }

    /// Channel count: 2 when the stereo bit is set, otherwise 1.
    pub fn channels(self) -> u8 {
        if self.stereo {
            2
        } else {
            1
        }
    }
}

/// One `0x46` payload: a complete Opus packet, ready for the native decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePacket {
    /// The packet bytes, verbatim. This is what goes into `OpusVoiceDecoder` / `OpusDecoder`.
    pub payload: Vec<u8>,
    /// Its parsed header, or `None` if the packet is malformed. Malformed packets are still
    /// DELIVERED — a decoder that can make something of one should get the chance, and this
    /// module has no business dropping audio on the strength of a header parse.
    pub toc: Option<OpusToc>,
    /// Ordinal within the current capture, starting at 0. Lets the shell tell "the utterance
    /// had no audio" from "the utterance never opened", which both reference implementations conflate.
    pub index: u64,
}

/// Why a capture ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndCause {
    /// A `0x99` arrived. **Never observed** — see this module's docs. A session that waits for
    /// this hangs, which is why the other three exist.
    DeviceSignalled,
    /// The app wrote [`InterruptVoice`](crate::AppCommand::InterruptVoice) (`0x56`). The only
    /// close any capture actually contains.
    Interrupted,
    /// The shell's inter-frame idle timer expired. The DURATION is the shell's (client policy);
    /// this is only the name of the moment.
    IdleTimeout,
    /// The link dropped mid-capture.
    Disconnected,
}

/// What one inbound frame did to the capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// `0x97` — the microphone opened.
    Started,
    /// A `0x46` arrived with no preceding `0x97`: the start frame was lost and the capture was
    /// opened implicitly, rather than dropping the utterance.
    StartedRecovered,
    /// One Opus packet. Emitted in the same call as the start event when a lost `0x97` was
    /// recovered, so ordering within the returned list is meaningful.
    Packet(VoicePacket),
    /// The capture closed.
    Ended(EndCause),
    /// Not a voice frame, or a close with nothing open. Reported rather than silently swallowed
    /// so a router's coverage can be asserted.
    Ignored,
}

/// The `0x97` → `0x46`… → close state machine.
///
/// Sans-IO: this owns a boolean and a counter. It cannot notice that the stream went quiet
/// because it has no clock — the shell times that and calls [`VoiceStream::close`].
#[derive(Debug, Clone, Default)]
pub struct VoiceStream {
    capturing: bool,
    packets: u64,
}

impl VoiceStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a capture is open.
    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// How many `0x46` packets the current (or most recent) capture carried.
    pub fn packets_in_capture(&self) -> u64 {
        self.packets
    }

    /// Drive the machine with one decoded `AC 55` frame.
    ///
    /// **Control-channel frames only.** A `52 58` file frame carries `0x97` and `0x99` with
    /// entirely different meanings; feeding one here opens and closes a microphone capture every
    /// time a photo transfers. [`crate::reassembly`] is the other half of that pair.
    pub fn handle(&mut self, frame: &Frame) -> Vec<VoiceEvent> {
        self.handle_upload(frame.cmd, &frame.data)
    }

    /// [`Self::handle`] for a caller that has the parts but not the struct.
    pub fn handle_upload(&mut self, cmd: u8, data: &[u8]) -> Vec<VoiceEvent> {
        let mut events = Vec::with_capacity(2);
        match DeviceUpload::from_code(cmd) {
            Some(DeviceUpload::VoiceUploadStart) => {
                // A `0x97` mid-capture restarts rather than continues: the device is announcing
                // a new utterance, and carrying the old packet count into it would misreport
                // both. Both reference implementations behave the same way.
                self.capturing = true;
                self.packets = 0;
                events.push(VoiceEvent::Started);
            }
            Some(DeviceUpload::VoiceData) => {
                if !self.capturing {
                    self.capturing = true;
                    self.packets = 0;
                    events.push(VoiceEvent::StartedRecovered);
                }
                // An empty payload is not a packet. It has never been captured — every `0x46`
                // carries 40 bytes — but emitting a zero-length packet would push the decision
                // into the native decoder, which is where a crash would be hardest to read.
                if !data.is_empty() {
                    events.push(VoiceEvent::Packet(VoicePacket {
                        payload: data.to_vec(),
                        toc: OpusToc::parse(data),
                        index: self.packets,
                    }));
                    self.packets += 1;
                }
            }
            Some(DeviceUpload::VoiceUploadEnd) => {
                return self.close(EndCause::DeviceSignalled);
            }
            _ => events.push(VoiceEvent::Ignored),
        }
        events
    }

    /// Close the capture for a reason the shell knows and this module cannot: the `0x56` write
    /// went out, an idle timer expired, or the link dropped.
    ///
    /// Returns [`VoiceEvent::Ended`] when a capture was open and [`VoiceEvent::Ignored`] when
    /// none was. Both reference implementations return nothing at all here, so on firmware that never sends `0x99`
    /// — which is all of it — an utterance had no end signal from any source.
    pub fn close(&mut self, cause: EndCause) -> Vec<VoiceEvent> {
        if !self.capturing {
            return vec![VoiceEvent::Ignored];
        }
        self.capturing = false;
        vec![VoiceEvent::Ended(cause)]
    }

    /// Forget the capture without emitting anything. For a shell tearing down state it has
    /// already reported on; prefer [`Self::close`], which says what happened.
    pub fn reset(&mut self) {
        self.capturing = false;
        self.packets = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_device, encode_device};

    /// Assemble a TOC byte the way RFC 6716 §3.1 lays it out — `ccccc s ff`. Written as an
    /// expression rather than a binary literal so the three fields are named at every call site.
    fn toc_byte(config: u8, stereo: bool, code: u8) -> u8 {
        (config << 3) | (u8::from(stereo) << 2) | code
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            s.len().is_multiple_of(2),
            "hex literal must be byte-aligned"
        );
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Whole `AC 55` frames lifted verbatim out of the packet captures with
    /// `tshark -r <capture>.pklg -Y btatt.value`, one per distinct packet SHAPE observed. Every
    /// capture produces the same three shapes and no others.
    const GOLDEN_VOICE: &[(&str, &str)] = &[
        (
            "ac55002a464b410516aed2dea71b251b88ee731dbd78b1d8b1c98fd34a354f35\
             796e124b53043ab2000000000077",
            "client_1 — code 3, padding 5, the commonest shape (14,119 of 14,636 frames)",
        ),
        (
            "ac55002a464b019ca544f9aea00f176a93d906d385190e623189e2faa7bd27df\
             592ad108cf4b20d192754fe1cfae",
            "client_1 — code 3, no padding: the frame filled all 38 remaining bytes",
        ),
        (
            "ac55002a46488cd3f2a67e0522d31c62b63bc6746db5ad8d28e5e6cc475b44e9\
             710c1f7c74976b7b5fff84a990b4",
            "client_1 — code 0, the single-frame short form (517 of 14,636)",
        ),
        (
            "ac55002a464b41028c2a8a973dc990e8f89abdeb001c9fa30599585d03bf6cf6\
             835dd38b1c3d1c752d88e00000f5",
            "eyevue_2 — code 3, padding 2, a different unit and a different vendor app",
        ),
        (
            "ac55002a464b410901d5d128801c1297a2caa577e3bfa3912d5d533268a799f8\
             a70b332400000000000000000005",
            "client_2 — code 3, padding 9",
        ),
        (
            "ac55002a464b410513971b2ab32e5c65e2a1b0a9e70c7938063d2b0e2e17d32e\
             d6c1faefeb6ed5800000000000d8",
            "eyevue_1 — code 3, padding 5",
        ),
    ];

    /// The captured `0x97`, byte-identical wherever it appears.
    const GOLDEN_START: &str = "ac550003970198";

    /// The load-bearing test: every captured voice frame is one well-formed Opus packet whose
    /// header says SILK, wideband, 20 ms, mono, one frame — and the padding arithmetic accounts
    /// for all 40 bytes with none left over. That is what turns the reference implementations' "格式为opus" comment
    /// from a quotation into a measurement.
    #[test]
    fn every_captured_voice_frame_is_one_wideband_silk_packet_of_twenty_milliseconds() {
        for (h, why) in GOLDEN_VOICE {
            let bytes = hex(h);
            let frame = decode_device(&bytes).unwrap_or_else(|e| panic!("{why}: {e:?}"));
            assert_eq!(frame.cmd, DeviceUpload::VoiceData.code(), "{why}");
            assert_eq!(frame.data.len(), CAPTURED_PACKET_BYTES, "{why}");
            assert_eq!(encode_device(frame.cmd, &frame.data), bytes, "{why}");

            let toc = OpusToc::parse(&frame.data).unwrap_or_else(|| panic!("{why}: no TOC"));
            assert_eq!(toc.config, 9, "{why}");
            assert_eq!(toc.mode(), OpusMode::Silk, "{why}");
            assert_eq!(toc.bandwidth(), OpusBandwidth::Wide, "{why}");
            assert_eq!(
                toc.bandwidth().nominal_rate_hz(),
                REQUESTED_SAMPLE_RATE_HZ,
                "{why}"
            );
            assert_eq!(toc.frame_duration_us(), 20_000, "{why}");
            assert_eq!(toc.total_duration_us(), Some(20_000), "{why}");
            assert!(!toc.stereo, "{why}");
            assert_eq!(toc.channels() as u32, CHANNELS, "{why}");
            assert_eq!(
                toc.frames, 1,
                "no captured packet carries more than one frame: {why}"
            );
            assert!(!toc.vbr, "{why}");
            // 20 ms at 16 kHz is 320 samples, i.e. 640 bytes of PCM16 mono per frame.
            assert_eq!(
                toc.decoded_samples(REQUESTED_SAMPLE_RATE_HZ),
                Some(320),
                "{why}"
            );

            // The padding fields account for the fixed 40-byte length exactly. TOC (1) +
            // frame-count byte when code 3 (1) + padding-length byte when padded (1) + the
            // compressed frame + the padding == 40, with nothing unexplained.
            if toc.code == 3 {
                let header = if toc.padding { 3 } else { 2 };
                let pad = if toc.padding {
                    frame.data[2] as usize
                } else {
                    0
                };
                assert!(header + pad <= CAPTURED_PACKET_BYTES, "{why}");
                let audio = CAPTURED_PACKET_BYTES - header - pad;
                assert!(
                    audio > 0,
                    "the padding must not consume the whole packet: {why}"
                );
                assert!(
                    frame.data[CAPTURED_PACKET_BYTES - pad..]
                        .iter()
                        .all(|b| *b == 0),
                    "Opus padding is zero-filled — the trailing zeroes are padding, not \
                     silence and not truncation: {why}"
                );
            } else {
                assert_eq!(
                    toc.code, 0,
                    "only codes 0 and 3 appear in any capture: {why}"
                );
            }
        }
    }

    /// The whole captured shape, driven through the machine as the radio delivers it.
    #[test]
    fn a_captured_start_then_voice_frames_produces_packets_in_order() {
        let mut v = VoiceStream::new();
        assert!(!v.is_capturing());

        let start = decode_device(&hex(GOLDEN_START)).expect("captured 0x97 decodes");
        assert_eq!(start.data, vec![0x01], "the start frame's payload is 0x01");
        assert_eq!(v.handle(&start), vec![VoiceEvent::Started]);
        assert!(v.is_capturing());

        for (i, (h, why)) in GOLDEN_VOICE.iter().enumerate() {
            let frame = decode_device(&hex(h)).expect("captured 0x46 decodes");
            let events = v.handle(&frame);
            match events.as_slice() {
                [VoiceEvent::Packet(p)] => {
                    assert_eq!(p.index, i as u64, "{why}");
                    assert_eq!(
                        p.payload, frame.data,
                        "the payload reaches the shell verbatim"
                    );
                    assert!(p.toc.is_some(), "{why}");
                }
                other => panic!("{why}: {other:?}"),
            }
        }
        assert_eq!(v.packets_in_capture(), GOLDEN_VOICE.len() as u64);
    }

    /// A lost `0x97` must not cost the utterance — the first `0x46` opens the capture, and says
    /// that it did so.
    #[test]
    fn a_lost_start_frame_is_recovered_by_the_first_voice_frame() {
        let mut v = VoiceStream::new();
        let frame = decode_device(&hex(GOLDEN_VOICE[0].0)).expect("decodes");
        let events = v.handle(&frame);
        assert!(
            matches!(
                events.as_slice(),
                [VoiceEvent::StartedRecovered, VoiceEvent::Packet(_)]
            ),
            "{events:?}"
        );
        assert!(v.is_capturing());
        // The recovery happens once; the next frame is just a packet.
        let events = v.handle(&frame);
        assert!(
            matches!(events.as_slice(), [VoiceEvent::Packet(p)] if p.index == 1),
            "{events:?}"
        );
    }

    /// The behaviour both reference implementations are missing. `0x99` is never sent, so a capture that is only
    /// ended by a `0x99` is never ended at all; a `0x56` write, an idle timer and a disconnect
    /// each close it here and each say which one it was.
    #[test]
    fn every_way_a_capture_can_end_produces_an_end_event() {
        for cause in [
            EndCause::Interrupted,
            EndCause::IdleTimeout,
            EndCause::Disconnected,
        ] {
            let mut v = VoiceStream::new();
            let start = decode_device(&hex(GOLDEN_START)).expect("decodes");
            v.handle(&start);
            assert_eq!(v.close(cause), vec![VoiceEvent::Ended(cause)]);
            assert!(!v.is_capturing());
            // Closing twice is not an error, but it is not a second ending either.
            assert_eq!(v.close(cause), vec![VoiceEvent::Ignored]);
        }
    }

    /// The `0x99` path still exists and still works — it is simply never exercised by this
    /// firmware, and the evidence level in [`crate::opcodes`] says so.
    #[test]
    fn a_device_signalled_end_is_handled_but_marked_as_never_observed() {
        use crate::Evidence;
        assert_eq!(
            DeviceUpload::VoiceUploadEnd.evidence(),
            Evidence::ClientOnly
        );
        assert_eq!(DeviceUpload::VoiceData.evidence(), Evidence::Capture);
        assert_eq!(DeviceUpload::VoiceUploadStart.evidence(), Evidence::Capture);

        let mut v = VoiceStream::new();
        v.handle_upload(DeviceUpload::VoiceUploadStart.code(), &[0x01]);
        let end = encode_device(DeviceUpload::VoiceUploadEnd.code(), &[]);
        let frame = decode_device(&end).expect("decodes");
        assert_eq!(
            v.handle(&frame),
            vec![VoiceEvent::Ended(EndCause::DeviceSignalled)]
        );
        assert!(!v.is_capturing());
    }

    /// A close with nothing open is ignored, not an implicit start.
    #[test]
    fn an_end_frame_with_no_capture_open_is_ignored() {
        let mut v = VoiceStream::new();
        assert_eq!(
            v.handle_upload(DeviceUpload::VoiceUploadEnd.code(), &[0x00]),
            vec![VoiceEvent::Ignored]
        );
        assert!(!v.is_capturing());
    }

    /// A second `0x97` restarts the utterance rather than continuing it, so the packet index
    /// counts within the current capture and not since the link came up.
    #[test]
    fn a_second_start_frame_restarts_the_packet_count() {
        let mut v = VoiceStream::new();
        let start = decode_device(&hex(GOLDEN_START)).expect("decodes");
        let voice = decode_device(&hex(GOLDEN_VOICE[0].0)).expect("decodes");
        v.handle(&start);
        v.handle(&voice);
        v.handle(&voice);
        assert_eq!(v.packets_in_capture(), 2);
        assert_eq!(v.handle(&start), vec![VoiceEvent::Started]);
        assert_eq!(v.packets_in_capture(), 0);
        let events = v.handle(&voice);
        assert!(
            matches!(events.as_slice(), [VoiceEvent::Packet(p)] if p.index == 0),
            "{events:?}"
        );
    }

    /// Frames that are not part of the voice stream are reported as ignored rather than
    /// silently absorbed — the battery push and the capability word both share this channel.
    #[test]
    fn non_voice_frames_are_ignored() {
        let mut v = VoiceStream::new();
        for h in [
            "ac55000453013185",
            "ac550006950000040ba4",
            "ac55000442000f51",
        ] {
            let frame = decode_device(&hex(h)).expect("decodes");
            assert_eq!(v.handle(&frame), vec![VoiceEvent::Ignored], "{h}");
        }
        assert!(
            !v.is_capturing(),
            "an unrelated frame must not open a capture"
        );
    }

    /// The file stream reuses `0x97` and `0x99` for entirely different things. Pinned as a test
    /// so a future router keyed on the cmd byte alone fails here rather than in the field.
    #[test]
    fn the_voice_opcodes_collide_with_the_file_opcodes_by_value() {
        use crate::reassembly::FileOpcode;
        assert_eq!(
            DeviceUpload::VoiceUploadStart.code(),
            FileOpcode::Info.code()
        );
        assert_eq!(DeviceUpload::VoiceUploadEnd.code(), FileOpcode::End.code());
        assert_eq!(
            DeviceUpload::from_code(FileOpcode::Data.code()),
            None,
            "0x98 is file-only"
        );
    }

    /// An empty `0x46` is not a packet. Never captured — every one carries 40 bytes — but the
    /// alternative is handing a zero-length buffer to a platform codec.
    #[test]
    fn an_empty_voice_payload_opens_the_capture_but_emits_no_packet() {
        let mut v = VoiceStream::new();
        assert_eq!(
            v.handle_upload(DeviceUpload::VoiceData.code(), &[]),
            vec![VoiceEvent::StartedRecovered]
        );
        assert!(v.is_capturing());
        assert_eq!(v.packets_in_capture(), 0);
    }

    /// A malformed packet is still delivered: dropping audio on the strength of a header parse
    /// is this module deciding something the native decoder is better placed to decide.
    #[test]
    fn a_malformed_packet_is_delivered_with_no_toc() {
        // Code 3 with no frame-count byte — the one truncation the header itself can detect.
        assert_eq!(OpusToc::parse(&[0x4B]), None);
        assert_eq!(OpusToc::parse(&[]), None);

        let mut v = VoiceStream::new();
        v.handle_upload(DeviceUpload::VoiceUploadStart.code(), &[0x01]);
        let events = v.handle_upload(DeviceUpload::VoiceData.code(), &[0x4B]);
        match events.as_slice() {
            [VoiceEvent::Packet(p)] => {
                assert_eq!(p.payload, vec![0x4B]);
                assert_eq!(p.toc, None, "the geometry is unknown, the bytes are not");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The RFC 6716 §3.1 configuration table, covered end to end. It is a table, so the only
    /// way it can be wrong is a transcription slip, and the only way to catch one is to write
    /// the expected values out separately.
    #[test]
    fn the_opus_configuration_table_matches_rfc_6716() {
        // (config, mode, bandwidth, frame duration µs)
        let expected: [(u8, OpusMode, OpusBandwidth, u32); 32] = [
            (0, OpusMode::Silk, OpusBandwidth::Narrow, 10_000),
            (1, OpusMode::Silk, OpusBandwidth::Narrow, 20_000),
            (2, OpusMode::Silk, OpusBandwidth::Narrow, 40_000),
            (3, OpusMode::Silk, OpusBandwidth::Narrow, 60_000),
            (4, OpusMode::Silk, OpusBandwidth::Medium, 10_000),
            (5, OpusMode::Silk, OpusBandwidth::Medium, 20_000),
            (6, OpusMode::Silk, OpusBandwidth::Medium, 40_000),
            (7, OpusMode::Silk, OpusBandwidth::Medium, 60_000),
            (8, OpusMode::Silk, OpusBandwidth::Wide, 10_000),
            (9, OpusMode::Silk, OpusBandwidth::Wide, 20_000),
            (10, OpusMode::Silk, OpusBandwidth::Wide, 40_000),
            (11, OpusMode::Silk, OpusBandwidth::Wide, 60_000),
            (12, OpusMode::Hybrid, OpusBandwidth::SuperWide, 10_000),
            (13, OpusMode::Hybrid, OpusBandwidth::SuperWide, 20_000),
            (14, OpusMode::Hybrid, OpusBandwidth::Full, 10_000),
            (15, OpusMode::Hybrid, OpusBandwidth::Full, 20_000),
            (16, OpusMode::Celt, OpusBandwidth::Narrow, 2_500),
            (17, OpusMode::Celt, OpusBandwidth::Narrow, 5_000),
            (18, OpusMode::Celt, OpusBandwidth::Narrow, 10_000),
            (19, OpusMode::Celt, OpusBandwidth::Narrow, 20_000),
            (20, OpusMode::Celt, OpusBandwidth::Wide, 2_500),
            (21, OpusMode::Celt, OpusBandwidth::Wide, 5_000),
            (22, OpusMode::Celt, OpusBandwidth::Wide, 10_000),
            (23, OpusMode::Celt, OpusBandwidth::Wide, 20_000),
            (24, OpusMode::Celt, OpusBandwidth::SuperWide, 2_500),
            (25, OpusMode::Celt, OpusBandwidth::SuperWide, 5_000),
            (26, OpusMode::Celt, OpusBandwidth::SuperWide, 10_000),
            (27, OpusMode::Celt, OpusBandwidth::SuperWide, 20_000),
            (28, OpusMode::Celt, OpusBandwidth::Full, 2_500),
            (29, OpusMode::Celt, OpusBandwidth::Full, 5_000),
            (30, OpusMode::Celt, OpusBandwidth::Full, 10_000),
            (31, OpusMode::Celt, OpusBandwidth::Full, 20_000),
        ];
        for (config, mode, bandwidth, us) in expected {
            let toc = OpusToc::parse(&[toc_byte(config, false, 0)]).expect("code-0 packet parses");
            assert_eq!(toc.config, config);
            assert_eq!(toc.mode(), mode, "config {config}");
            assert_eq!(toc.bandwidth(), bandwidth, "config {config}");
            assert_eq!(toc.frame_duration_us(), us, "config {config}");
            assert_eq!(
                toc.decoded_samples(bandwidth.nominal_rate_hz()),
                Some((us as u64 * bandwidth.nominal_rate_hz() as u64 / 1_000_000) as usize),
                "config {config}"
            );
        }
    }

    /// The four frame-count codes. Only 0 and 3 have ever been captured; 1 and 2 are here
    /// because the RFC defines them and a packet using one would otherwise decode as garbage.
    #[test]
    fn the_frame_count_codes_decode_per_the_rfc() {
        // Code 0 — one frame.
        let t = OpusToc::parse(&[toc_byte(9, false, 0)]).expect("code 0");
        assert_eq!((t.code, t.frames, t.vbr, t.padding), (0, 1, false, false));
        // Code 1 — two frames of equal size, CBR.
        let t = OpusToc::parse(&[toc_byte(9, false, 1)]).expect("code 1");
        assert_eq!((t.code, t.frames, t.vbr, t.padding), (1, 2, false, false));
        // Code 2 — two frames, the first's length coded explicitly.
        let t = OpusToc::parse(&[toc_byte(9, false, 2)]).expect("code 2");
        assert_eq!((t.code, t.frames, t.vbr, t.padding), (2, 2, true, false));
        // Code 3 — the count, VBR flag and padding flag all come from the next byte.
        let t = OpusToc::parse(&[toc_byte(9, false, 3), 0b1100_0011]).expect("code 3");
        assert_eq!((t.code, t.frames, t.vbr, t.padding), (3, 3, true, true));
        assert_eq!(t.total_duration_us(), Some(60_000), "three 20 ms frames");
        // A zero frame count is malformed; the RFC requires at least one.
        let t = OpusToc::parse(&[toc_byte(9, false, 3), 0b0000_0000]).expect("code 3");
        assert_eq!(t.frames, 0);
        assert_eq!(t.total_duration_us(), None);
        assert_eq!(t.decoded_samples(48_000), None);
        // The stereo bit is bit 2 and is independent of everything else.
        let t = OpusToc::parse(&[toc_byte(9, true, 0)]).expect("stereo");
        assert!(t.stereo);
        assert_eq!(t.channels(), 2);
    }

    /// The longest packet the format allows must not overflow the duration arithmetic — 48
    /// frames of 60 ms is 2.88 s, and computing it in `u32` microseconds is fine, but the
    /// sample count at 48 kHz is not.
    #[test]
    fn the_largest_legal_packet_does_not_overflow_the_sample_count() {
        let t = OpusToc::parse(&[toc_byte(3, false, 3), 0b0011_0000]).expect("code 3, 48 frames");
        assert_eq!((t.config, t.frames), (3, 48));
        assert_eq!(t.frame_duration_us(), 60_000);
        assert_eq!(t.total_duration_us(), Some(2_880_000));
        assert_eq!(t.decoded_samples(48_000), Some(138_240));
    }
}
