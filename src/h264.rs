//! H.264 over RTP (RFC 6184) → Annex B access units.
//!
//! The glasses send Main profile, level 5.2, 1600×1200 on dynamic payload type 96, with SPS and
//! PPS **in-band** ahead of every IDR rather than in the SDP. Three of RFC 6184's packet types
//! appear on this wire and no others:
//!
//! | payload byte `& 0x1F` | RFC 6184 | what the glasses put in it |
//! |---|---|---|
//! | 1–23 | a single NAL unit | small NALs — SEI, and non-IDR slices that fit an MTU |
//! | 24 | STAP-A | SPS + PPS, as one 33-byte packet, immediately before each IDR |
//! | 28 | FU-A | every slice too big for the MTU, in ~1430-byte fragments |
//!
//! ## What a decoder needs, and what this hands it
//!
//! Output is **Annex B**: every NAL prefixed with `00 00 00 01`, NALs of one picture
//! concatenated into one [`AccessUnit`]. Write those bytes to a file and `ffplay` opens it;
//! feed them to `VideoToolbox`/`MediaCodec` after converting to length-prefixed AVCC if that is
//! what the decoder wants.
//!
//! An access unit ends when the RTP **marker bit** is set. The timestamp changing also ends
//! one, as a backstop for a lost final packet — without it a dropped marker glues two pictures
//! together and the decoder shows one corrupt frame rather than dropping one.
//!
//! ## Starting a file in the right place
//!
//! A recording that begins mid-GOP is a grey mess until the next IDR. [`Depacketizer`] reports
//! [`AccessUnit::is_decodable_start`] — an IDR that arrived with its SPS and PPS — and keeps a
//! [`Depacketizer::keyframe_seen`] flag, so a writer can throw away everything before the first
//! one. That is exactly what `examples/live.rs` does.
//!
//! ## A lost fragment start is dropped, not guessed
//!
//! An FU-A continuation with no start (the S-bit packet was lost) cannot be reassembled: the
//! original NAL header lived in that packet. The fragment is dropped and counted in
//! [`DepacketizerStats::dropped_fragments`] rather than emitted headless, which is what
//! produces a decoder error a long way from the cause.

use crate::rtp::{self, RtpHeader};

/// The four-byte Annex B start code. Three-byte codes are legal; this writes the four-byte one
/// everywhere, which is what every muxer and player accepts.
pub const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Coded slice of a non-IDR picture.
pub const NAL_NON_IDR: u8 = 1;
/// Coded slice of an IDR picture — a keyframe.
pub const NAL_IDR: u8 = 5;
/// Supplemental enhancement information.
pub const NAL_SEI: u8 = 6;
/// Sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// Picture parameter set.
pub const NAL_PPS: u8 = 8;
/// Access unit delimiter.
pub const NAL_AUD: u8 = 9;

/// RFC 6184 packet type: single-time aggregation packet, type A.
pub const PACKET_STAP_A: u8 = 24;
/// RFC 6184 packet type: fragmentation unit, type A.
pub const PACKET_FU_A: u8 = 28;

/// The NAL unit type from a NAL header byte — the low five bits.
pub fn nal_type(header_byte: u8) -> u8 {
    header_byte & 0x1F
}

/// `nal_ref_idc` — the two bits above the type. Zero means the decoder may discard the NAL.
pub fn nal_ref_idc(header_byte: u8) -> u8 {
    (header_byte >> 5) & 0x03
}

/// Whether a NAL type is a parameter set (SPS or PPS).
pub fn is_parameter_set(nal_type: u8) -> bool {
    nal_type == NAL_SPS || nal_type == NAL_PPS
}

/// Whether a NAL type is an IDR slice.
pub fn is_keyframe_nal(nal_type: u8) -> bool {
    nal_type == NAL_IDR
}

/// One picture, in Annex B, ready to write or decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// The RTP timestamp of every packet that made it. 90 kHz on this stream, so
    /// `timestamp / 90000` is seconds.
    pub timestamp: u32,
    /// Annex B bytes: `00 00 00 01 <nal> 00 00 00 01 <nal> …`.
    pub data: Vec<u8>,
    /// The NAL types in it, in order. Cheap to inspect without re-scanning `data`.
    pub nal_types: Vec<u8>,
    /// Whether the marker bit closed this unit. `false` means the timestamp changed instead,
    /// which is the lost-marker path and a sign of loss.
    pub closed_by_marker: bool,
}

impl AccessUnit {
    /// Whether this unit contains an IDR slice.
    pub fn is_keyframe(&self) -> bool {
        self.nal_types.iter().copied().any(is_keyframe_nal)
    }

    /// Whether it carries both parameter sets.
    pub fn has_parameter_sets(&self) -> bool {
        self.nal_types.contains(&NAL_SPS) && self.nal_types.contains(&NAL_PPS)
    }

    /// Whether a decoder handed THIS unit first can decode from here: an IDR with its SPS and
    /// PPS. On this stream that is every keyframe, because the STAP-A precedes each one.
    pub fn is_decodable_start(&self) -> bool {
        self.is_keyframe() && self.has_parameter_sets()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Running counts. Cheap, and the only way to tell "the link is bad" from "the code is wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DepacketizerStats {
    pub access_units: u64,
    pub keyframes: u64,
    pub nal_units: u64,
    pub bytes: u64,
    /// FU-A continuations with no start packet. See the module docs.
    pub dropped_fragments: u64,
    /// Packets whose type this depacketiser does not implement (STAP-B, MTAP, FU-B) or that
    /// were malformed. Counted rather than logged, because at 25 fps a log is a firehose.
    pub unsupported_packets: u64,
    /// Access units closed by a timestamp change rather than a marker bit.
    pub units_closed_without_marker: u64,
}

/// RFC 6184 depacketiser: RTP packets in, Annex B access units out.
///
/// Sans-IO. It holds bytes and counters, has no clock, and cannot decide that a stream has
/// stalled — a client times that and calls [`Depacketizer::reset`].
#[derive(Debug, Clone, Default)]
pub struct Depacketizer {
    timestamp: Option<u32>,
    nals: Vec<Vec<u8>>,
    nal_types: Vec<u8>,
    fu: Option<FragmentState>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    keyframe_seen: bool,
    stats: DepacketizerStats,
}

#[derive(Debug, Clone)]
struct FragmentState {
    /// The reconstructed NAL, header byte included, growing as fragments arrive.
    buf: Vec<u8>,
    nal_type: u8,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one whole RTP datagram.
    ///
    /// A datagram that is not RTP, or is RTP of another payload type, yields nothing and is
    /// counted in [`DepacketizerStats::unsupported_packets`] — the port can receive stray
    /// traffic and a hard error there would kill a working stream.
    pub fn push(&mut self, packet: &[u8]) -> Vec<AccessUnit> {
        match rtp::parse(packet) {
            Ok(p) => self.push_payload(&p.header, p.payload),
            Err(_) => {
                self.stats.unsupported_packets += 1;
                Vec::new()
            }
        }
    }

    /// Feed a payload whose RTP header a caller already parsed — for a client that demultiplexes
    /// video and audio itself.
    pub fn push_payload(&mut self, header: &RtpHeader, payload: &[u8]) -> Vec<AccessUnit> {
        let mut out = Vec::new();

        // A new timestamp closes whatever was open. Backstop for a lost marker.
        if let Some(ts) = self.timestamp {
            if ts != header.timestamp {
                if self.fu.take().is_some() {
                    self.stats.dropped_fragments += 1;
                }
                if let Some(au) = self.emit(ts, false) {
                    self.stats.units_closed_without_marker += 1;
                    out.push(au);
                }
            }
        }
        self.timestamp = Some(header.timestamp);

        if payload.is_empty() {
            self.stats.unsupported_packets += 1;
        } else {
            self.consume(payload);
        }

        if header.marker {
            if self.fu.take().is_some() {
                // A marker inside an unfinished fragment: the tail was lost.
                self.stats.dropped_fragments += 1;
            }
            if let Some(au) = self.emit(header.timestamp, true) {
                out.push(au);
            }
            self.timestamp = None;
        }
        out
    }

    fn consume(&mut self, payload: &[u8]) {
        let kind = nal_type(payload[0]);
        match kind {
            1..=23 => self.push_nal(payload.to_vec()),
            PACKET_STAP_A => self.consume_stap_a(&payload[1..]),
            PACKET_FU_A => self.consume_fu_a(payload),
            // STAP-B (25), MTAP16 (26), MTAP24 (27), FU-B (29), and the reserved 0/30/31.
            // None has ever appeared on this stream.
            _ => self.stats.unsupported_packets += 1,
        }
    }

    fn consume_stap_a(&mut self, mut rest: &[u8]) {
        loop {
            if rest.is_empty() {
                return;
            }
            if rest.len() < 2 {
                self.stats.unsupported_packets += 1;
                return;
            }
            let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            if size == 0 || rest.len() < 2 + size {
                self.stats.unsupported_packets += 1;
                return;
            }
            self.push_nal(rest[2..2 + size].to_vec());
            rest = &rest[2 + size..];
        }
    }

    fn consume_fu_a(&mut self, payload: &[u8]) {
        if payload.len() < 3 {
            self.stats.unsupported_packets += 1;
            return;
        }
        let indicator = payload[0];
        let fu_header = payload[1];
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        let kind = fu_header & 0x1F;
        let body = &payload[2..];

        if start {
            if self.fu.is_some() {
                // A new start while one was open: the previous NAL's tail never arrived.
                self.stats.dropped_fragments += 1;
            }
            // The original NAL header is rebuilt from the indicator's F/NRI bits and the FU
            // header's type — it exists in neither byte alone.
            let nal_header = (indicator & 0xE0) | kind;
            let mut buf = Vec::with_capacity(body.len() + 1);
            buf.push(nal_header);
            buf.extend_from_slice(body);
            self.fu = Some(FragmentState {
                buf,
                nal_type: kind,
            });
        } else {
            match self.fu.as_mut() {
                Some(f) => f.buf.extend_from_slice(body),
                None => {
                    // The start packet was lost; the NAL header is unrecoverable.
                    self.stats.dropped_fragments += 1;
                    return;
                }
            }
        }

        if end {
            if let Some(f) = self.fu.take() {
                debug_assert_eq!(nal_type(f.buf[0]), f.nal_type);
                self.push_nal(f.buf);
            }
        }
    }

    fn push_nal(&mut self, nal: Vec<u8>) {
        if nal.is_empty() {
            return;
        }
        let kind = nal_type(nal[0]);
        match kind {
            NAL_SPS => self.sps = Some(nal.clone()),
            NAL_PPS => self.pps = Some(nal.clone()),
            _ => {}
        }
        self.stats.nal_units += 1;
        self.nal_types.push(kind);
        self.nals.push(nal);
    }

    fn emit(&mut self, timestamp: u32, closed_by_marker: bool) -> Option<AccessUnit> {
        if self.nals.is_empty() {
            self.nal_types.clear();
            return None;
        }
        let nals = core::mem::take(&mut self.nals);
        let nal_types = core::mem::take(&mut self.nal_types);
        let mut data = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for n in &nals {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(n);
        }
        let au = AccessUnit {
            timestamp,
            data,
            nal_types,
            closed_by_marker,
        };
        self.stats.access_units += 1;
        self.stats.bytes += au.data.len() as u64;
        if au.is_keyframe() {
            self.stats.keyframes += 1;
        }
        if au.is_decodable_start() {
            self.keyframe_seen = true;
        }
        Some(au)
    }

    /// Close whatever is open and hand it over. For end-of-stream; a partial picture is better
    /// evidence than a silently discarded one.
    pub fn flush(&mut self) -> Option<AccessUnit> {
        let ts = self.timestamp.take()?;
        self.fu = None;
        self.emit(ts, false)
    }

    /// Whether a unit with an IDR *and* its parameter sets has been emitted. A recorder should
    /// drop everything before the first one.
    pub fn keyframe_seen(&self) -> bool {
        self.keyframe_seen
    }

    /// The most recent SPS and PPS, as raw NALs (no start code).
    ///
    /// For a decoder that wants them out of band, or a writer that joined a stream late and
    /// wants to prepend them to the first IDR it keeps.
    pub fn parameter_sets(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        (self.sps.as_deref(), self.pps.as_deref())
    }

    /// The parameter sets in Annex B, ready to write ahead of a picture. Empty until both have
    /// been seen.
    pub fn parameter_sets_annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for set in [self.sps.as_deref(), self.pps.as_deref()]
            .into_iter()
            .flatten()
        {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(set);
        }
        out
    }

    pub fn stats(&self) -> DepacketizerStats {
        self.stats
    }

    /// Drop every partial NAL and picture, keep the statistics. For a stream that stalled or an
    /// SSRC that changed.
    pub fn reset(&mut self) {
        self.timestamp = None;
        self.nals.clear();
        self.nal_types.clear();
        self.fu = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The STAP-A that opens a keyframe, cut whole from a live session. SPS (12 bytes) and PPS
    /// (4 bytes) in one 33-byte datagram.
    const LIVE_STAP_A: [u8; 33] = [
        0x80, 0x60, 0x81, 0x80, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, 0x78, 0x00, 0x0C,
        0x67, 0x4D, 0x00, 0x34, 0x96, 0x54, 0x03, 0x20, 0x12, 0xF4, 0x88, 0x08, 0x00, 0x04, 0x68,
        0xEE, 0x38, 0x80,
    ];

    /// The three FU-A packets that carried the IDR after it, headers cut from the same session.
    /// The fragment BODIES are truncated to eight bytes each so the vectors stay readable —
    /// the real datagrams were 1442, 1442 and 1313 bytes. Every header byte is verbatim: the
    /// sequence numbers, the shared 90 kHz timestamp, the S/E bits and the marker.
    const LIVE_FU_START: [u8; 22] = [
        0x80, 0x60, 0x81, 0x81, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, 0x7C, 0x85, 0xB8,
        0x04, 0x00, 0xFF, 0xFD, 0xE6, 0x0D, 0x27,
    ];
    const LIVE_FU_MIDDLE: [u8; 22] = [
        0x80, 0x60, 0x81, 0x82, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, 0x7C, 0x05, 0xC1,
        0x80, 0xDB, 0xD9, 0x6F, 0xC8, 0xD5, 0xCF,
    ];
    const LIVE_FU_END: [u8; 22] = [
        0x80, 0xE0, 0x81, 0xAC, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, 0x7C, 0x45, 0xF5,
        0x35, 0xE9, 0xA3, 0xC4, 0xB4, 0x57, 0x6F,
    ];

    #[test]
    fn a_real_keyframe_arrives_as_one_access_unit_of_sps_pps_and_idr() {
        let mut d = Depacketizer::new();
        assert!(d.push(&LIVE_STAP_A).is_empty(), "no marker yet");
        assert!(d.push(&LIVE_FU_START).is_empty());
        assert!(d.push(&LIVE_FU_MIDDLE).is_empty());
        let units = d.push(&LIVE_FU_END);

        assert_eq!(units.len(), 1, "the marker bit closed exactly one picture");
        let au = &units[0];
        assert_eq!(au.timestamp, 1_744_464);
        assert_eq!(au.nal_types, vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(au.is_keyframe());
        assert!(au.has_parameter_sets());
        assert!(au.is_decodable_start());
        assert!(au.closed_by_marker);
        assert!(d.keyframe_seen());

        // Annex B, four-byte start codes, SPS first.
        assert_eq!(&au.data[..4], &START_CODE);
        assert_eq!(au.data[4], 0x67);
        assert_eq!(&au.data[4..16], &LIVE_STAP_A[15..27], "the SPS, verbatim");
        assert_eq!(&au.data[16..20], &START_CODE);
        assert_eq!(&au.data[20..24], &[0x68, 0xEE, 0x38, 0x80], "the PPS");
        assert_eq!(&au.data[24..28], &START_CODE);
        assert_eq!(
            au.data[28], 0x65,
            "IDR, nal_ref_idc 3 — rebuilt from the FU bytes"
        );
        assert_eq!(nal_type(au.data[28]), NAL_IDR);
        assert_eq!(nal_ref_idc(au.data[28]), 3);

        // 8 body bytes from the start, 8 from the middle, 8 from the end, plus the header.
        let idr_len = au.data.len() - 28;
        assert_eq!(idr_len, 1 + 8 + 8 + 8);

        let s = d.stats();
        assert_eq!(s.access_units, 1);
        assert_eq!(s.keyframes, 1);
        assert_eq!(s.nal_units, 3);
        assert_eq!(s.dropped_fragments, 0);
        assert_eq!(s.unsupported_packets, 0);
    }

    #[test]
    fn the_parameter_sets_are_kept_for_a_writer_that_joined_late() {
        let mut d = Depacketizer::new();
        d.push(&LIVE_STAP_A);
        let (sps, pps) = d.parameter_sets();
        assert_eq!(sps.unwrap()[0], 0x67);
        assert_eq!(pps.unwrap(), &[0x68, 0xEE, 0x38, 0x80]);
        assert_eq!(d.parameter_sets_annex_b().len(), 4 + 12 + 4 + 4);
    }

    #[test]
    fn a_fragment_whose_start_was_lost_is_dropped_rather_than_emitted_headless() {
        let mut d = Depacketizer::new();
        d.push(&LIVE_STAP_A);
        // The S-bit packet never arrives.
        assert!(d.push(&LIVE_FU_MIDDLE).is_empty());
        let units = d.push(&LIVE_FU_END);
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].nal_types,
            vec![NAL_SPS, NAL_PPS],
            "the slice is gone; the parameter sets are not"
        );
        assert!(!units[0].is_keyframe());
        assert!(!d.keyframe_seen(), "a writer must not start here");
        assert_eq!(d.stats().dropped_fragments, 2, "the middle and the end");
    }

    #[test]
    fn a_single_nal_packet_becomes_one_annex_b_nal() {
        // Synthetic, built to RFC 6184: a 6-byte SEI as its own packet, marker set.
        let mut pkt = vec![0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x10, 0x00, 1, 2, 3, 4];
        pkt.extend_from_slice(&[0x06, 0x05, 0x01, 0xAA, 0x80]);
        let mut d = Depacketizer::new();
        let units = d.push(&pkt);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].nal_types, vec![NAL_SEI]);
        assert_eq!(
            units[0].data,
            [&START_CODE[..], &[0x06, 0x05, 0x01, 0xAA, 0x80]].concat()
        );
    }

    #[test]
    fn a_lost_marker_still_closes_the_picture_when_the_timestamp_moves_on() {
        let mut d = Depacketizer::new();
        d.push(&LIVE_STAP_A);
        d.push(&LIVE_FU_START);
        // The E-bit/marker packet is lost, and the next picture starts.
        let mut next = LIVE_STAP_A;
        next[4..8].copy_from_slice(&1_748_064u32.to_be_bytes());
        let units = d.push(&next);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 1_744_464);
        assert!(!units[0].closed_by_marker);
        assert_eq!(d.stats().units_closed_without_marker, 1);
        assert_eq!(d.stats().dropped_fragments, 1, "the unfinished slice");
    }

    #[test]
    fn a_packet_type_this_stream_never_sends_is_counted_and_not_decoded() {
        let mut d = Depacketizer::new();
        // FU-B (29) and a truncated STAP-A.
        let header = [0x80u8, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut fu_b = header.to_vec();
        fu_b.extend_from_slice(&[0x7D, 0x85, 0x00, 0x00, 0xAA]);
        assert!(d.push(&fu_b).is_empty());

        let mut short_stap = header.to_vec();
        short_stap.extend_from_slice(&[0x78, 0x00, 0xFF, 0x01]); // declares 255, has 1
        assert!(d.push(&short_stap).is_empty());

        // Not RTP at all — a stray datagram on the port.
        assert!(d.push(b"hello").is_empty());
        assert_eq!(d.stats().unsupported_packets, 3);
        assert_eq!(d.stats().nal_units, 0);
    }

    #[test]
    fn flush_hands_over_a_picture_whose_marker_never_came() {
        let mut d = Depacketizer::new();
        d.push(&LIVE_STAP_A);
        let au = d
            .flush()
            .expect("the parameter sets are still worth having");
        assert_eq!(au.nal_types, vec![NAL_SPS, NAL_PPS]);
        assert!(!au.closed_by_marker);
        assert!(d.flush().is_none(), "nothing left");
    }

    #[test]
    fn the_nal_helpers_agree_with_the_constants() {
        assert_eq!(nal_type(0x67), NAL_SPS);
        assert_eq!(nal_type(0x68), NAL_PPS);
        assert_eq!(nal_type(0x65), NAL_IDR);
        assert_eq!(nal_type(0x41), NAL_NON_IDR);
        assert!(is_parameter_set(NAL_SPS) && is_parameter_set(NAL_PPS));
        assert!(!is_parameter_set(NAL_IDR));
        assert!(is_keyframe_nal(NAL_IDR) && !is_keyframe_nal(NAL_NON_IDR));
        assert_eq!(nal_ref_idc(0x67), 3);
        assert_eq!(nal_ref_idc(0x01), 0);
    }
}
