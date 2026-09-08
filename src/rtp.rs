//! RTP (RFC 3550) — the twelve-byte header, and a sequence tracker that notices loss.
//!
//! The live stream arrives as UDP datagrams on the client port named in SETUP, one RTP packet
//! per datagram. This module parses the header and hands back the payload; what the payload
//! MEANS is [`crate::h264`]'s and [`crate::aac`]'s.
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X|  CC   |M|     PT      |       sequence number         |
//! |                           timestamp                           |
//! |                             SSRC                              |
//! |                          CSRC (0..15)                         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! What the glasses actually send, from a live session: `V=2`, no padding, no extension, no
//! CSRC, one SSRC for the whole session, payload type 96 for video (97 for audio when track1 is
//! set up), and the marker bit set on the LAST packet of each access unit and nowhere else.
//! `80 60` and `80 E0` are therefore the only two first-two-byte pairs on the video track.
//!
//! ## Sequence numbers wrap, and that is not loss
//!
//! [`SequenceTracker`] compares in 16-bit modular arithmetic (RFC 3550 §A.1's rule): a
//! difference in the top half of the space is a REORDER, not a 65,000-packet gap. Getting this
//! wrong produces a loss counter that reads fine for eighteen minutes at 25 fps and then
//! reports 99 % loss for one packet, which is how it presents in a stats overlay.

use core::fmt;

/// The fixed part of the header, before any CSRC list or extension.
pub const HEADER_BYTES: usize = 12;

/// The version this parser accepts. RFC 3550 has not moved.
pub const VERSION: u8 = 2;

/// A parsed RTP header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    /// Set on the last packet of an access unit. The only frame boundary this stream carries
    /// other than the timestamp changing.
    pub marker: bool,
    /// 96 video / 97 audio here — dynamic, and meaningless without the SDP.
    pub payload_type: u8,
    pub sequence: u16,
    /// 90 kHz on the video track, 16 kHz on audio (the rtpmap clock rate).
    pub timestamp: u32,
    pub ssrc: u32,
    /// Offset of the payload within the packet: past the fixed header, the CSRC list and any
    /// extension.
    pub payload_offset: usize,
    /// Padding bytes trimmed from the end, per the `P` bit.
    pub padding_bytes: usize,
}

/// A header and the payload it introduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    pub header: RtpHeader,
    /// Payload with the extension skipped and the padding trimmed.
    pub payload: &'a [u8],
}

/// Why a datagram is not an RTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpError {
    /// Shorter than the twelve-byte fixed header.
    TooShort { len: usize },
    /// Version field was not 2. Almost always a datagram from something else on the port.
    BadVersion { version: u8 },
    /// The CSRC count ran past the end of the datagram.
    TruncatedCsrc,
    /// The `X` bit was set and the extension header does not fit.
    TruncatedExtension,
    /// The `P` bit was set and the padding length is zero or longer than the payload.
    BadPadding { stated: usize, available: usize },
}

impl fmt::Display for RtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtpError::TooShort { len } => write!(f, "{len} bytes, need {HEADER_BYTES}"),
            RtpError::BadVersion { version } => write!(f, "RTP version {version}, expected 2"),
            RtpError::TruncatedCsrc => write!(f, "CSRC list runs past the packet"),
            RtpError::TruncatedExtension => write!(f, "extension header runs past the packet"),
            RtpError::BadPadding { stated, available } => {
                write!(f, "padding of {stated} in {available} bytes")
            }
        }
    }
}

impl std::error::Error for RtpError {}

/// Parse one datagram as an RTP packet.
///
/// The extension header, if present, is SKIPPED rather than returned: nothing this stream
/// carries uses one, and a parser that returns it invites a caller to treat it as payload.
pub fn parse(bytes: &[u8]) -> Result<RtpPacket<'_>, RtpError> {
    if bytes.len() < HEADER_BYTES {
        return Err(RtpError::TooShort { len: bytes.len() });
    }
    let version = bytes[0] >> 6;
    if version != VERSION {
        return Err(RtpError::BadVersion { version });
    }
    let padding = bytes[0] & 0x20 != 0;
    let extension = bytes[0] & 0x10 != 0;
    let csrc_count = bytes[0] & 0x0F;

    let marker = bytes[1] & 0x80 != 0;
    let payload_type = bytes[1] & 0x7F;
    let sequence = u16::from_be_bytes([bytes[2], bytes[3]]);
    let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let ssrc = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

    let mut offset = HEADER_BYTES + csrc_count as usize * 4;
    if offset > bytes.len() {
        return Err(RtpError::TruncatedCsrc);
    }
    if extension {
        if offset + 4 > bytes.len() {
            return Err(RtpError::TruncatedExtension);
        }
        let words = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 4 + words * 4;
        if offset > bytes.len() {
            return Err(RtpError::TruncatedExtension);
        }
    }

    let mut end = bytes.len();
    let mut padding_bytes = 0;
    if padding {
        let stated = *bytes.last().expect("non-empty, checked above") as usize;
        let available = end - offset;
        if stated == 0 || stated > available {
            return Err(RtpError::BadPadding { stated, available });
        }
        padding_bytes = stated;
        end -= stated;
    }

    Ok(RtpPacket {
        header: RtpHeader {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            payload_offset: offset,
            padding_bytes,
        },
        payload: &bytes[offset..end],
    })
}

/// What one sequence number meant relative to the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceReport {
    /// The first packet of the stream, or the first after a [`SequenceTracker::reset`].
    First,
    /// Exactly one past the previous highest. The normal case.
    InOrder,
    /// Ahead of the previous highest, with a gap. `missing` counts the numbers skipped — they
    /// may still arrive later as [`Self::Reordered`], which is why `lost` is an estimate.
    Lost { missing: u16 },
    /// Behind the previous highest: a late or reordered packet, still usable.
    Reordered { behind: u16 },
    /// A sequence number already seen. UDP duplicates happen; decoding one twice does not.
    Duplicate,
}

/// Counts packets, gaps and reorders on one SSRC.
///
/// Sans-IO like everything else: it holds three integers and a small window, has no clock and
/// cannot decide that a gap has become permanent. A client that wants "loss in the last second"
/// samples [`SequenceTracker::lost_estimate`] on its own timer.
#[derive(Debug, Clone)]
pub struct SequenceTracker {
    highest: Option<u16>,
    received: u64,
    lost: i64,
    duplicates: u64,
    reordered: u64,
    /// Bitmap of the 64 sequence numbers below `highest`, so a late arrival can retire a gap
    /// instead of double-counting it.
    window: u64,
}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self {
            highest: None,
            received: 0,
            lost: 0,
            duplicates: 0,
            reordered: 0,
            window: 0,
        }
    }

    /// The highest sequence number seen, in modular order.
    pub fn highest(&self) -> Option<u16> {
        self.highest
    }

    /// Packets accepted, duplicates excluded.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Packets believed missing. An ESTIMATE: a gap is counted when it opens and retired when a
    /// late packet fills it, so this can go down. It never goes below zero.
    pub fn lost_estimate(&self) -> u64 {
        self.lost.max(0) as u64
    }

    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    pub fn reordered(&self) -> u64 {
        self.reordered
    }

    /// Loss as a fraction of what should have arrived, `0.0..=1.0`. Zero before any packet.
    pub fn loss_fraction(&self) -> f64 {
        let expected = self.received + self.lost_estimate();
        if expected == 0 {
            0.0
        } else {
            self.lost_estimate() as f64 / expected as f64
        }
    }

    /// Forget everything. Called on an SSRC change, which is a new stream wearing the old port.
    pub fn reset(&mut self) {
        *self = SequenceTracker::new();
    }

    /// Record one sequence number and say what it was.
    pub fn observe(&mut self, sequence: u16) -> SequenceReport {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            self.received = 1;
            return SequenceReport::First;
        };

        // RFC 3550 §A.1: a difference in the top half of the 16-bit space is a step BACKWARDS,
        // not a jump forward of 32,000-odd. This is the whole wrap-around question.
        let delta = sequence.wrapping_sub(highest);
        if delta == 0 {
            self.duplicates += 1;
            return SequenceReport::Duplicate;
        }
        if delta < 0x8000 {
            // Forward.
            let missing = delta - 1;
            self.received += 1;
            self.highest = Some(sequence);
            self.window = if delta >= 64 {
                0
            } else {
                (self.window << delta) | (1u64 << (delta - 1))
            };
            if missing > 0 {
                self.lost += missing as i64;
                return SequenceReport::Lost { missing };
            }
            SequenceReport::InOrder
        } else {
            // Backward: late or duplicate.
            let behind = highest.wrapping_sub(sequence);
            if behind <= 64 {
                let bit = 1u64 << (behind - 1);
                if self.window & bit != 0 {
                    self.duplicates += 1;
                    return SequenceReport::Duplicate;
                }
                self.window |= bit;
            }
            self.received += 1;
            self.reordered += 1;
            self.lost -= 1;
            SequenceReport::Reordered { behind }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One whole datagram from a live session: the STAP-A that opens every keyframe. 33 bytes,
    /// which is the 12-byte header plus a 21-byte payload.
    const LIVE_STAP_A: [u8; 33] = [
        0x80, 0x60, 0x81, 0x80, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, // header
        0x78, 0x00, 0x0C, 0x67, 0x4D, 0x00, 0x34, 0x96, 0x54, 0x03, 0x20, 0x12, 0xF4, 0x88, 0x08,
        0x00, 0x04, 0x68, 0xEE, 0x38, 0x80,
    ];

    /// The packet after it — the first fragment of the IDR, same timestamp. Truncated here to
    /// its first eight payload bytes; the full datagram was 1442 bytes.
    const LIVE_FU_A_START: [u8; 22] = [
        0x80, 0x60, 0x81, 0x81, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, // header
        0x7C, 0x85, 0xB8, 0x04, 0x00, 0xFF, 0xFD, 0xE6, 0x0D, 0x27,
    ];

    /// The last fragment of that same IDR — note `0xE0`: the marker bit is set here and on no
    /// other packet of the frame. Truncated the same way.
    const LIVE_FU_A_END: [u8; 22] = [
        0x80, 0xE0, 0x81, 0xAC, 0x00, 0x1A, 0x9E, 0x50, 0x7B, 0x7A, 0x79, 0x78, // header
        0x7C, 0x45, 0xF5, 0x35, 0xE9, 0xA3, 0xC4, 0xB4, 0x57, 0x6F,
    ];

    #[test]
    fn a_real_video_packet_parses_into_the_fields_the_depacketiser_needs() {
        let p = parse(&LIVE_STAP_A).expect("an RTP packet");
        assert_eq!(p.header.version, 2);
        assert!(!p.header.padding);
        assert!(!p.header.extension);
        assert_eq!(p.header.csrc_count, 0);
        assert!(!p.header.marker);
        assert_eq!(p.header.payload_type, 96);
        assert_eq!(p.header.sequence, 33_152);
        assert_eq!(p.header.timestamp, 1_744_464);
        assert_eq!(p.header.ssrc, 0x7B7A7978);
        assert_eq!(p.header.payload_offset, HEADER_BYTES);
        assert_eq!(p.payload.len(), 21);
        assert_eq!(p.payload[0], 0x78, "a STAP-A");
    }

    #[test]
    fn the_marker_bit_is_set_on_the_last_packet_of_a_frame_and_not_before_it() {
        let start = parse(&LIVE_FU_A_START).unwrap();
        let end = parse(&LIVE_FU_A_END).unwrap();
        assert!(!start.header.marker);
        assert!(end.header.marker);
        // Same frame: same timestamp, same SSRC, consecutive-ish sequence.
        assert_eq!(start.header.timestamp, end.header.timestamp);
        assert_eq!(start.header.ssrc, end.header.ssrc);
        assert_eq!(end.header.sequence - start.header.sequence, 43);
    }

    #[test]
    fn a_datagram_from_something_else_on_the_port_is_rejected_by_version() {
        assert_eq!(parse(&[0u8; 4]), Err(RtpError::TooShort { len: 4 }));
        let mut bad = LIVE_STAP_A;
        bad[0] = 0x40; // version 1
        assert_eq!(parse(&bad), Err(RtpError::BadVersion { version: 1 }));
    }

    #[test]
    fn a_csrc_list_and_an_extension_are_both_skipped_to_find_the_payload() {
        // Synthetic — this server sends neither — built to the RFC so the offsets are pinned.
        let mut pkt = vec![0x82, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]; // CC=2
        pkt.extend_from_slice(&[0xAA; 8]); // two CSRC entries
        pkt.extend_from_slice(b"payload");
        let p = parse(&pkt).unwrap();
        assert_eq!(p.header.csrc_count, 2);
        assert_eq!(p.payload, b"payload");

        let mut ext = vec![0x90, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]; // X=1
        ext.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01, 1, 2, 3, 4]); // one 32-bit word
        ext.extend_from_slice(b"payload");
        let p = parse(&ext).unwrap();
        assert!(p.header.extension);
        assert_eq!(p.payload, b"payload");
        assert_eq!(parse(&ext[..14]), Err(RtpError::TruncatedExtension));
    }

    #[test]
    fn padding_is_trimmed_and_a_lying_padding_length_is_refused() {
        let mut pkt = vec![0xA0, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]; // P=1
        pkt.extend_from_slice(b"abc");
        pkt.extend_from_slice(&[0, 0, 3]); // three bytes of padding, count last
        let p = parse(&pkt).unwrap();
        assert_eq!(p.payload, b"abc");
        assert_eq!(p.header.padding_bytes, 3);

        let mut lying = pkt.clone();
        *lying.last_mut().unwrap() = 99;
        assert!(matches!(parse(&lying), Err(RtpError::BadPadding { .. })));
    }

    #[test]
    fn a_clean_run_of_sequence_numbers_reports_no_loss() {
        let mut t = SequenceTracker::new();
        assert_eq!(t.observe(33_152), SequenceReport::First);
        for seq in 33_153..33_200 {
            assert_eq!(t.observe(seq), SequenceReport::InOrder);
        }
        assert_eq!(t.received(), 48);
        assert_eq!(t.lost_estimate(), 0);
        assert_eq!(t.loss_fraction(), 0.0);
        assert_eq!(t.highest(), Some(33_199));
    }

    #[test]
    fn a_gap_is_counted_when_it_opens_and_retired_when_the_late_packet_lands() {
        let mut t = SequenceTracker::new();
        t.observe(10);
        assert_eq!(t.observe(13), SequenceReport::Lost { missing: 2 });
        assert_eq!(t.lost_estimate(), 2);
        assert_eq!(t.observe(11), SequenceReport::Reordered { behind: 2 });
        assert_eq!(t.observe(12), SequenceReport::Reordered { behind: 1 });
        assert_eq!(t.lost_estimate(), 0, "both gaps filled");
        assert_eq!(t.reordered(), 2);
        assert_eq!(t.observe(12), SequenceReport::Duplicate);
        assert_eq!(t.duplicates(), 1);
    }

    /// At 25 fps and a handful of packets per frame this wrap happens every few minutes. A
    /// tracker that reads it as a forward jump reports 65,000 lost packets each time.
    #[test]
    fn the_sequence_number_wrapping_past_65535_is_not_loss() {
        let mut t = SequenceTracker::new();
        t.observe(65_534);
        assert_eq!(t.observe(65_535), SequenceReport::InOrder);
        assert_eq!(t.observe(0), SequenceReport::InOrder);
        assert_eq!(t.observe(1), SequenceReport::InOrder);
        assert_eq!(t.lost_estimate(), 0);
        assert_eq!(t.highest(), Some(1));
        // A number already seen across the wrap is a duplicate, not a 65,000-packet leap.
        assert_eq!(t.observe(65_535), SequenceReport::Duplicate);

        // And a genuinely late packet across the wrap is a reorder that retires its own gap.
        let mut t = SequenceTracker::new();
        t.observe(65_534);
        assert_eq!(t.observe(0), SequenceReport::Lost { missing: 1 });
        assert_eq!(t.observe(65_535), SequenceReport::Reordered { behind: 1 });
        assert_eq!(t.lost_estimate(), 0);
    }

    #[test]
    fn loss_fraction_is_the_share_of_what_should_have_arrived() {
        let mut t = SequenceTracker::new();
        t.observe(0);
        t.observe(2); // one lost
        assert_eq!(t.received(), 2);
        assert_eq!(t.lost_estimate(), 1);
        assert!((t.loss_fraction() - 1.0 / 3.0).abs() < 1e-9);
        t.reset();
        assert_eq!(t.loss_fraction(), 0.0);
        assert_eq!(t.highest(), None);
    }
}
