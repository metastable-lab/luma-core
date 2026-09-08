//! AAC over RTP (RFC 3640, mode `AAC-hbr`) → raw AAC frames, and an ADTS header to wrap them.
//!
//! The glasses advertise a second track beside the video: `mpeg4-generic/16000/1`, dynamic
//! payload type 97, `config=1408`. That config is a two-byte MPEG-4 AudioSpecificConfig and it
//! decodes to **AAC-LC, 16 kHz, mono** — see [`parse_config_hex`].
//!
//! ```text
//! a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408;
//!           sizeLength=13; indexlength=3; indexdeltalength=3
//! ```
//!
//! ## The payload layout
//!
//! ```text
//! +---------------+----------------------+-------------------+
//! | AU-headers-   |  AU-header (1..n)    |   AU data (1..n)  |
//! | length (16 b) |  13 b size + 3 b idx |                   |
//! +---------------+----------------------+-------------------+
//! ```
//!
//! `AU-headers-length` is in **BITS, not bytes** — the single most common misreading of
//! RFC 3640, and it is off by a factor of eight, so it produces a "frame" the decoder rejects
//! rather than an obvious crash. With `sizeLength=13` and `indexLength=3` each header is 16
//! bits, so a one-frame packet declares `0x0010` and not `0x0002`.
//!
//! The header block is padded to a byte boundary before the AU data starts. At 13+3 bits there
//! is never any padding on this stream, but the arithmetic is done properly so a firmware that
//! changes `sizeLength` does not silently shift every frame by a byte.
//!
//! ## Making the frames playable
//!
//! Raw AAC frames are not a file. [`adts_header`] builds the seven-byte ADTS header that turns
//! each one into a self-describing unit; concatenate `header + frame` per frame and the result
//! plays in `ffplay`, VLC or QuickTime as `.aac`.
//!
//! ## Evidence
//!
//! The SDP line above is verbatim from a live session. The RTP audio PACKETS are not: every
//! capture set up `track0` only, so no audio ever came down the wire. The depacketiser and the
//! ADTS builder are written to RFC 3640 and ISO/IEC 14496-3 and their tests use vectors
//! constructed from those documents — stated here rather than left for someone to discover.
//! The `config=1408` decode is checkable against the SDP itself and is pinned by a test.

use core::fmt;

/// The sampling frequency index table from ISO/IEC 14496-3. Index 8 is this stream's 16 kHz.
pub const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The `config` value this firmware advertises, as an integer.
pub const CONFIG_1408: u16 = 0x1408;

/// The sample rate the glasses' audio track runs at.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Channels on the glasses' audio track.
pub const CHANNELS: u8 = 1;

/// Samples in one AAC-LC frame. Fixed by the codec, not by this stream.
pub const SAMPLES_PER_FRAME: u32 = 1024;

/// Bytes in an ADTS header without the optional CRC. This builder never writes the CRC.
pub const ADTS_HEADER_BYTES: usize = 7;

/// The largest frame an ADTS header can describe: the length field is 13 bits and covers the
/// header too.
pub const MAX_ADTS_FRAME_BYTES: usize = (1 << 13) - 1;

/// A decoded MPEG-4 AudioSpecificConfig — the `config=` parameter, unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// `2` is AAC-LC. The ADTS `profile` field is this minus one.
    pub object_type: u8,
    /// Index into [`SAMPLE_RATES`].
    pub sampling_frequency_index: u8,
    /// 1 is mono, 2 is stereo.
    pub channel_configuration: u8,
    /// Resolved from the index, for convenience.
    pub sample_rate_hz: u32,
}

impl AudioSpecificConfig {
    /// The config the glasses advertise: AAC-LC, 16 kHz, mono.
    pub const GLASSES: AudioSpecificConfig = AudioSpecificConfig {
        object_type: 2,
        sampling_frequency_index: 8,
        channel_configuration: 1,
        sample_rate_hz: SAMPLE_RATE_HZ,
    };

    /// Seconds of audio one frame carries.
    pub fn frame_duration_secs(&self) -> f64 {
        SAMPLES_PER_FRAME as f64 / self.sample_rate_hz as f64
    }
}

/// Parse a two-byte AudioSpecificConfig.
///
/// Layout: 5 bits object type, 4 bits sampling frequency index, 4 bits channel configuration.
/// The escape value 15 for the frequency index (a 24-bit explicit rate follows) is refused
/// rather than guessed — nothing sends it here.
pub fn parse_audio_specific_config(bytes: &[u8]) -> Result<AudioSpecificConfig, AacError> {
    if bytes.len() < 2 {
        return Err(AacError::UnsupportedConfig);
    }
    let word = u16::from_be_bytes([bytes[0], bytes[1]]);
    let object_type = (word >> 11) as u8 & 0x1F;
    let sampling_frequency_index = (word >> 7) as u8 & 0x0F;
    let channel_configuration = (word >> 3) as u8 & 0x0F;
    if object_type == 0 || object_type == 31 {
        return Err(AacError::UnsupportedConfig);
    }
    let sample_rate_hz = *SAMPLE_RATES
        .get(sampling_frequency_index as usize)
        .ok_or(AacError::UnsupportedConfig)?;
    Ok(AudioSpecificConfig {
        object_type,
        sampling_frequency_index,
        channel_configuration,
        sample_rate_hz,
    })
}

/// Parse the SDP `config=` parameter, which is hex text (`"1408"`).
pub fn parse_config_hex(hex: &str) -> Result<AudioSpecificConfig, AacError> {
    let hex = hex.trim();
    if hex.len() < 4 || hex.len() % 2 != 0 {
        return Err(AacError::UnsupportedConfig);
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        bytes
            .push(u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| AacError::UnsupportedConfig)?);
    }
    parse_audio_specific_config(&bytes)
}

/// The three `fmtp` widths that decide how AU headers are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuHeaderConfig {
    /// Bits of AU size per header. 13 here.
    pub size_length: u8,
    /// Bits of AU index in the FIRST header. 3 here.
    pub index_length: u8,
    /// Bits of index delta in every header after the first. 3 here.
    pub index_delta_length: u8,
}

impl Default for AuHeaderConfig {
    /// The glasses' values, so a client that never reads the SDP still frames correctly.
    fn default() -> Self {
        AuHeaderConfig::GLASSES
    }
}

impl AuHeaderConfig {
    /// `sizeLength=13; indexLength=3; indexDeltaLength=3` — verbatim from the live SDP.
    pub const GLASSES: AuHeaderConfig = AuHeaderConfig {
        size_length: 13,
        index_length: 3,
        index_delta_length: 3,
    };

    /// Read the widths out of a parsed `a=fmtp:` line.
    ///
    /// Uses [`crate::sdp::Fmtp::get`], which is case-insensitive — the server spells two of the
    /// three keys in lower case and a case-sensitive read finds only `sizeLength`.
    pub fn from_fmtp(fmtp: &crate::sdp::Fmtp) -> Option<AuHeaderConfig> {
        Some(AuHeaderConfig {
            size_length: fmtp.size_length()? as u8,
            index_length: fmtp.index_length().unwrap_or(0) as u8,
            index_delta_length: fmtp.index_delta_length().unwrap_or(0) as u8,
        })
    }

    /// Bits in the first AU header.
    pub fn first_header_bits(&self) -> u32 {
        self.size_length as u32 + self.index_length as u32
    }

    /// Bits in every subsequent AU header.
    pub fn later_header_bits(&self) -> u32 {
        self.size_length as u32 + self.index_delta_length as u32
    }
}

/// One AU header: how big the access unit is and where it sits in the series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuHeader {
    /// Bytes of AU data this header describes.
    pub size: usize,
    /// The AU index, or the delta for headers after the first.
    pub index: u32,
}

/// Why an audio payload could not be unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AacError {
    /// Shorter than the two-byte AU-headers-length field.
    PayloadTooShort,
    /// The declared AU-headers-length does not fit the payload, or is not a whole number of
    /// headers. Usually the bits/bytes confusion — see the module docs.
    BadAuHeadersLength { bits: u32, payload_len: usize },
    /// The AU sizes add up to more data than the payload contains.
    TruncatedAu { declared: usize, available: usize },
    /// A frame too long for an ADTS length field.
    FrameTooLarge { len: usize },
    /// A `config=` value this crate will not guess at.
    UnsupportedConfig,
    /// A header configuration with a zero size field: every AU would be zero bytes.
    ZeroSizeLength,
}

impl fmt::Display for AacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AacError::PayloadTooShort => write!(f, "payload shorter than the AU-headers-length"),
            AacError::BadAuHeadersLength { bits, payload_len } => {
                write!(
                    f,
                    "AU-headers-length {bits} bits does not fit {payload_len} bytes"
                )
            }
            AacError::TruncatedAu {
                declared,
                available,
            } => {
                write!(
                    f,
                    "AU headers declare {declared} bytes, {available} present"
                )
            }
            AacError::FrameTooLarge { len } => write!(f, "{len} bytes will not fit an ADTS frame"),
            AacError::UnsupportedConfig => write!(f, "unsupported AudioSpecificConfig"),
            AacError::ZeroSizeLength => write!(f, "sizeLength is zero"),
        }
    }
}

impl std::error::Error for AacError {}

/// Split one RTP audio payload into its AU headers and raw AAC frames.
///
/// Frames are BORROWED from the payload — no copy. `headers[i]` describes `frames[i]`.
pub fn depacketize<'a>(
    payload: &'a [u8],
    cfg: &AuHeaderConfig,
) -> Result<(Vec<AuHeader>, Vec<&'a [u8]>), AacError> {
    if cfg.size_length == 0 {
        return Err(AacError::ZeroSizeLength);
    }
    if payload.len() < 2 {
        return Err(AacError::PayloadTooShort);
    }
    // BITS. Not bytes.
    let header_bits = u16::from_be_bytes([payload[0], payload[1]]) as u32;
    let header_bytes = header_bits.div_ceil(8) as usize;
    if 2 + header_bytes > payload.len() {
        return Err(AacError::BadAuHeadersLength {
            bits: header_bits,
            payload_len: payload.len(),
        });
    }

    let mut reader = BitReader::new(&payload[2..2 + header_bytes]);
    let mut headers = Vec::new();
    let mut consumed = 0u32;
    while consumed < header_bits {
        let bits = if headers.is_empty() {
            cfg.first_header_bits()
        } else {
            cfg.later_header_bits()
        };
        if consumed + bits > header_bits {
            return Err(AacError::BadAuHeadersLength {
                bits: header_bits,
                payload_len: payload.len(),
            });
        }
        let size = reader
            .read(cfg.size_length as u32)
            .ok_or(AacError::PayloadTooShort)? as usize;
        let index_bits = bits - cfg.size_length as u32;
        let index = reader.read(index_bits).ok_or(AacError::PayloadTooShort)?;
        headers.push(AuHeader { size, index });
        consumed += bits;
    }

    let mut frames = Vec::with_capacity(headers.len());
    let mut at = 2 + header_bytes;
    let declared: usize = headers.iter().map(|h| h.size).sum();
    if at + declared > payload.len() {
        return Err(AacError::TruncatedAu {
            declared,
            available: payload.len() - at,
        });
    }
    for h in &headers {
        frames.push(&payload[at..at + h.size]);
        at += h.size;
    }
    Ok((headers, frames))
}

/// RTP audio packets in, AAC frames out.
///
/// Stateless apart from the configuration and a frame counter — RFC 3640 interleaving is not
/// implemented, because `mode=AAC-hbr` with no `constantDuration`/`maxDisplacement` (which is
/// what this server sends) is by definition non-interleaved.
#[derive(Debug, Clone, Default)]
pub struct Depacketizer {
    cfg: AuHeaderConfig,
    frames: u64,
    bytes: u64,
    errors: u64,
}

impl Depacketizer {
    /// With the glasses' header widths.
    pub fn new() -> Self {
        Self::default()
    }

    /// With widths read from the SDP.
    pub fn with_config(cfg: AuHeaderConfig) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    pub fn config(&self) -> AuHeaderConfig {
        self.cfg
    }

    /// Feed one whole RTP datagram; get the AAC frames out of it.
    ///
    /// A datagram that is not RTP, or a payload that will not unpack, yields nothing and bumps
    /// [`Depacketizer::errors`]. Same reasoning as the video path: a stray packet on the port
    /// must not kill the stream.
    pub fn push(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        match crate::rtp::parse(packet) {
            Ok(p) => self.push_payload(p.payload),
            Err(_) => {
                self.errors += 1;
                Vec::new()
            }
        }
    }

    /// Feed a payload whose RTP header a caller already stripped.
    pub fn push_payload(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        match depacketize(payload, &self.cfg) {
            Ok((_, frames)) => {
                self.frames += frames.len() as u64;
                self.bytes += frames.iter().map(|f| f.len() as u64).sum::<u64>();
                frames.into_iter().map(<[u8]>::to_vec).collect()
            }
            Err(_) => {
                self.errors += 1;
                Vec::new()
            }
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn errors(&self) -> u64 {
        self.errors
    }
}

/// Build the seven-byte ADTS header for one frame of `frame_len` raw AAC bytes.
///
/// `frame_len` is the AAC payload; the length written into the header is `frame_len + 7`,
/// because ADTS counts itself. No CRC (`protection_absent = 1`), one AAC frame per ADTS frame,
/// buffer fullness set to the "variable rate" escape `0x7FF`.
pub fn adts_header(
    config: &AudioSpecificConfig,
    frame_len: usize,
) -> Result<[u8; ADTS_HEADER_BYTES], AacError> {
    let total = frame_len + ADTS_HEADER_BYTES;
    if total > MAX_ADTS_FRAME_BYTES {
        return Err(AacError::FrameTooLarge { len: frame_len });
    }
    if config.object_type == 0 {
        return Err(AacError::UnsupportedConfig);
    }
    let profile = config.object_type - 1; // ADTS profile is objectType - 1
    let freq = config.sampling_frequency_index & 0x0F;
    let chan = config.channel_configuration & 0x07;
    let len = total as u32;
    Ok([
        0xFF,
        0xF1, // syncword, MPEG-4, layer 0, protection absent
        (profile << 6) | (freq << 2) | (chan >> 2),
        ((chan & 0x03) << 6) | ((len >> 11) & 0x03) as u8,
        ((len >> 3) & 0xFF) as u8,
        (((len & 0x07) as u8) << 5) | 0x1F, // buffer fullness, top 5 bits of 0x7FF
        0xFC,                               // rest of the fullness, 1 frame per ADTS frame
    ])
}

/// One AAC frame wrapped as a standalone ADTS frame — header and payload, ready to write.
pub fn adts_frame(config: &AudioSpecificConfig, frame: &[u8]) -> Result<Vec<u8>, AacError> {
    let header = adts_header(config, frame.len())?;
    let mut out = Vec::with_capacity(ADTS_HEADER_BYTES + frame.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(frame);
    Ok(out)
}

/// Read the frame length back out of an ADTS header — the inverse of [`adts_header`], for a
/// reader walking a `.aac` file.
pub fn adts_frame_length(header: &[u8]) -> Option<usize> {
    if header.len() < ADTS_HEADER_BYTES || header[0] != 0xFF || header[1] & 0xF0 != 0xF0 {
        return None;
    }
    let len = ((header[3] as usize & 0x03) << 11)
        | ((header[4] as usize) << 3)
        | (header[5] as usize >> 5);
    Some(len)
}

/// A big-endian bit reader over the AU header block.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        if count == 0 {
            return Some(0);
        }
        if count > 32 || self.bit + count as usize > self.bytes.len() * 8 {
            return None;
        }
        let mut out = 0u32;
        for _ in 0..count {
            let byte = self.bytes[self.bit / 8];
            let bit = (byte >> (7 - (self.bit % 8))) & 1;
            out = (out << 1) | bit as u32;
            self.bit += 1;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `config=` value from the live SDP. Everything else about this track follows from it.
    #[test]
    fn config_1408_is_aac_lc_sixteen_kilohertz_mono() {
        let c = parse_config_hex("1408").expect("a config");
        assert_eq!(c.object_type, 2, "AAC-LC");
        assert_eq!(c.sampling_frequency_index, 8);
        assert_eq!(c.sample_rate_hz, 16_000);
        assert_eq!(c.channel_configuration, 1, "mono");
        assert_eq!(c, AudioSpecificConfig::GLASSES);
        assert_eq!(u16::from_be_bytes([0x14, 0x08]), CONFIG_1408);
        assert!((c.frame_duration_secs() - 0.064).abs() < 1e-9);
    }

    #[test]
    fn the_header_widths_come_out_of_the_live_fmtp_line_despite_its_lower_case_keys() {
        let sd = crate::sdp::parse(concat!(
            "v=0\r\nm=audio 0 RTP/AVP 97\r\n",
            "a=rtpmap:97 mpeg4-generic/16000/1\r\n",
            "a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408; sizeLength=13; ",
            "indexlength=3; indexdeltalength=3\r\n",
        ))
        .unwrap();
        let fmtp = sd.audio().unwrap().fmtp.clone().unwrap();
        assert_eq!(
            AuHeaderConfig::from_fmtp(&fmtp),
            Some(AuHeaderConfig::GLASSES)
        );
        assert_eq!(AuHeaderConfig::GLASSES.first_header_bits(), 16);
        assert_eq!(AuHeaderConfig::GLASSES.later_header_bits(), 16);
        assert_eq!(AuHeaderConfig::default(), AuHeaderConfig::GLASSES);
    }

    /// Built to RFC 3640 §3.3.6 — no audio RTP packet exists in any capture, because every
    /// session set up track0 only. See the module docs.
    fn rtp_audio(frames: &[&[u8]]) -> Vec<u8> {
        let cfg = AuHeaderConfig::GLASSES;
        let mut payload = Vec::new();
        let bits = (frames.len() as u32) * cfg.first_header_bits();
        payload.extend_from_slice(&(bits as u16).to_be_bytes());
        for (i, f) in frames.iter().enumerate() {
            // 13 bits of size then 3 bits of index/delta, big-endian, packing to 16 bits.
            let word = ((f.len() as u16) << 3) | if i == 0 { 0 } else { 1 };
            payload.extend_from_slice(&word.to_be_bytes());
        }
        for f in frames {
            payload.extend_from_slice(f);
        }
        let mut pkt = vec![
            0x80, 0xE1, 0x00, 0x0A, 0x00, 0x00, 0x04, 0x00, 0x11, 0x22, 0x33, 0x44,
        ];
        pkt.extend_from_slice(&payload);
        pkt
    }

    #[test]
    fn one_access_unit_in_a_packet_comes_back_as_one_frame() {
        let frame = [0x21u8, 0x1A, 0x8F, 0x40, 0x00, 0x7C];
        let pkt = rtp_audio(&[&frame]);
        let p = crate::rtp::parse(&pkt).unwrap();
        assert_eq!(p.header.payload_type, 97);
        assert_eq!(
            &p.payload[..2],
            &[0x00, 0x10],
            "16 BITS of AU header, not 16 bytes"
        );

        let (headers, frames) = depacketize(p.payload, &AuHeaderConfig::GLASSES).unwrap();
        assert_eq!(headers, vec![AuHeader { size: 6, index: 0 }]);
        assert_eq!(frames, vec![&frame[..]]);
    }

    #[test]
    fn several_access_units_in_one_packet_split_at_the_declared_sizes() {
        let a = [0x01u8, 0x02, 0x03];
        let b = [0x04u8; 5];
        let c = [0x05u8; 2];
        let pkt = rtp_audio(&[&a, &b, &c]);
        let mut d = Depacketizer::new();
        let frames = d.push(&pkt);
        assert_eq!(frames, vec![a.to_vec(), b.to_vec(), c.to_vec()]);
        assert_eq!(d.frames(), 3);
        assert_eq!(d.bytes(), 10);
        assert_eq!(d.errors(), 0);
    }

    /// The bits/bytes confusion, stated as a test: a client that writes the field in BYTES
    /// produces a payload this parser refuses rather than one it mis-frames.
    #[test]
    fn a_headers_length_written_in_bytes_instead_of_bits_is_refused() {
        let frame = [0xAAu8; 6];
        let mut pkt = rtp_audio(&[&frame]);
        assert_eq!(
            &pkt[12..14],
            &[0x00, 0x10],
            "16 bits is what a correct writer sends"
        );
        pkt[12] = 0x00;
        pkt[13] = 0x02; // "two bytes", which reads as two BITS
        assert!(matches!(
            depacketize(&pkt[12..], &AuHeaderConfig::GLASSES),
            Err(AacError::BadAuHeadersLength { bits: 2, .. })
        ));
    }

    #[test]
    fn a_payload_that_promises_more_data_than_it_carries_is_an_error() {
        let cfg = AuHeaderConfig::GLASSES;
        // One header declaring 4096 bytes, four bytes present.
        let payload = [0x00, 0x10, 0x80, 0x00, 1, 2, 3, 4];
        assert_eq!(
            depacketize(&payload, &cfg),
            Err(AacError::TruncatedAu {
                declared: 4096,
                available: 4
            })
        );
        assert_eq!(depacketize(&[0x00], &cfg), Err(AacError::PayloadTooShort));
        assert!(matches!(
            depacketize(&[0xFF, 0xFF, 0x00], &cfg),
            Err(AacError::BadAuHeadersLength { .. })
        ));
        assert_eq!(
            depacketize(
                &payload,
                &AuHeaderConfig {
                    size_length: 0,
                    ..cfg
                }
            ),
            Err(AacError::ZeroSizeLength)
        );
    }

    #[test]
    fn a_stray_datagram_on_the_audio_port_is_counted_and_not_decoded() {
        let mut d = Depacketizer::new();
        assert!(d.push(b"not rtp").is_empty());
        assert_eq!(d.errors(), 1);
        assert_eq!(d.frames(), 0);
    }

    /// The header that makes a raw frame into a file a player will open.
    #[test]
    fn the_adts_header_describes_the_frame_plus_itself() {
        let frame = [0u8; 100];
        let h = adts_header(&AudioSpecificConfig::GLASSES, frame.len()).unwrap();
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1], 0xF1, "MPEG-4, layer 0, no CRC");
        // profile 1 (AAC-LC), freq index 8, channel 1.
        assert_eq!(h[2] >> 6, 1);
        assert_eq!((h[2] >> 2) & 0x0F, 8);
        assert_eq!(((h[2] & 0x01) << 2) | (h[3] >> 6), 1);
        assert_eq!(adts_frame_length(&h), Some(107), "100 + 7");
        let framed = adts_frame(&AudioSpecificConfig::GLASSES, &frame).unwrap();
        assert_eq!(framed.len(), 107);
        assert_eq!(&framed[..7], &h);
    }

    #[test]
    fn a_frame_too_long_for_the_thirteen_bit_length_field_is_refused() {
        assert_eq!(
            adts_header(&AudioSpecificConfig::GLASSES, MAX_ADTS_FRAME_BYTES),
            Err(AacError::FrameTooLarge {
                len: MAX_ADTS_FRAME_BYTES
            })
        );
        assert!(adts_header(&AudioSpecificConfig::GLASSES, MAX_ADTS_FRAME_BYTES - 7).is_ok());
    }

    #[test]
    fn a_config_this_crate_will_not_guess_at_is_refused() {
        assert_eq!(parse_config_hex("14"), Err(AacError::UnsupportedConfig));
        assert_eq!(parse_config_hex("zzzz"), Err(AacError::UnsupportedConfig));
        // Frequency index 15 is the "explicit rate follows" escape.
        assert_eq!(
            parse_audio_specific_config(&[0x17, 0x88]),
            Err(AacError::UnsupportedConfig)
        );
    }
}
