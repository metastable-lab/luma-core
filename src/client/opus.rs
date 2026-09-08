//! Wake-word packets → 16 kHz PCM, and a WAV file to put it in. Feature `opus`.
//!
//! The glasses stream the microphone as one complete Opus packet per `0x46` frame (§10) — SILK,
//! wideband, one 20 ms frame, mono, about fifty a second. [`crate::voice`] parses the packet
//! header and hands the packets over; this module decodes them.
//!
//! ```no_run
//! # #[cfg(feature = "opus")] {
//! use luma_core::client::opus::{VoiceDecoder, write_wav};
//! # let packets: Vec<luma_core::VoicePacket> = vec![];
//! let mut d = VoiceDecoder::new()?;
//! let pcm = d.decode_all(&packets)?;                 // i16, 16 kHz, mono
//! write_wav(std::path::Path::new("voice.wav"), &pcm)?;
//! # Ok::<(), Box<dyn std::error::Error>>(()) }
//! ```
//!
//! ## Why this is a separate feature
//!
//! The `opus` crate binds to the SYSTEM libopus (`brew install opus`, `apt install
//! libopus-dev`). That is a C library, a build script and a link step — everything the sans-IO
//! core exists to avoid. On iOS and Android there is a platform decoder already (CoreAudio,
//! `MediaCodec`) and this feature is the wrong answer; on a laptop it is the only one.
//!
//! ## Ask the decoder for 16 kHz, not for the packet's own rate
//!
//! The packets are SILK wideband, whose internal rate is 16 kHz, but Opus will resample to
//! whatever output rate the decoder was created with. [`REQUESTED_SAMPLE_RATE_HZ`] is what the
//! protocol reference recommends and what [`VoiceDecoder::new`] uses; a decoder created at
//! 48 kHz produces four times the samples and a WAV that plays at the right pitch and the wrong
//! size, which is a confusing thing to debug.
//!
//! ## Lost packets
//!
//! [`VoiceDecoder::decode_lost`] runs Opus' packet-loss concealment for one missing 20 ms frame.
//! The BLE link does not lose voice frames in any capture, so nothing calls it automatically —
//! but a client that tracks [`crate::VoicePacket::index`] and sees a gap has somewhere to go.

use std::io::Write;
use std::path::Path;

use crate::voice::{VoicePacket, CHANNELS, REQUESTED_SAMPLE_RATE_HZ};

/// Samples in one 20 ms frame at 16 kHz. The decode buffer is sized from this.
pub const SAMPLES_PER_FRAME: usize = (REQUESTED_SAMPLE_RATE_HZ as usize) / 50;

/// The largest number of samples one packet can decode to at 16 kHz: Opus allows up to 120 ms
/// per packet. Every captured packet is 20 ms; the buffer is sized for the maximum anyway
/// because a decode into a short buffer is an error, not a truncation.
pub const MAX_SAMPLES_PER_PACKET: usize = (REQUESTED_SAMPLE_RATE_HZ as usize) * 120 / 1000;

/// Anything that went wrong turning packets into audio.
#[derive(Debug)]
pub enum OpusError {
    /// libopus refused. Carries its own message.
    Codec { detail: String },
    /// A local filesystem failure while writing a WAV.
    Io { detail: String },
    /// More samples than a WAV's 32-bit size fields can describe.
    TooLong { samples: usize },
}

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpusError::Codec { detail } => write!(f, "opus: {detail}"),
            OpusError::Io { detail } => write!(f, "wav: {detail}"),
            OpusError::TooLong { samples } => write!(f, "{samples} samples will not fit a WAV"),
        }
    }
}

impl std::error::Error for OpusError {}

impl From<opus::Error> for OpusError {
    fn from(e: opus::Error) -> Self {
        OpusError::Codec {
            detail: e.to_string(),
        }
    }
}

impl From<std::io::Error> for OpusError {
    fn from(e: std::io::Error) -> Self {
        OpusError::Io {
            detail: e.to_string(),
        }
    }
}

/// A decoder for the glasses' wake-word stream.
///
/// One per capture. Opus is a stateful codec — the decoder carries the previous frame's state —
/// so decoding two utterances through one decoder without resetting joins them, and decoding one
/// utterance through two decoders loses the join.
pub struct VoiceDecoder {
    inner: opus::Decoder,
    buf: Vec<i16>,
}

impl VoiceDecoder {
    /// A mono decoder at [`REQUESTED_SAMPLE_RATE_HZ`].
    pub fn new() -> Result<VoiceDecoder, OpusError> {
        VoiceDecoder::at_rate(REQUESTED_SAMPLE_RATE_HZ)
    }

    /// A mono decoder at another output rate. See the module note before using this.
    pub fn at_rate(sample_rate_hz: u32) -> Result<VoiceDecoder, OpusError> {
        debug_assert_eq!(CHANNELS, 1, "the glasses' microphone is mono");
        Ok(VoiceDecoder {
            inner: opus::Decoder::new(sample_rate_hz, opus::Channels::Mono)?,
            buf: vec![0i16; MAX_SAMPLES_PER_PACKET],
        })
    }

    /// Decode one packet to PCM.
    pub fn decode_packet(&mut self, packet: &[u8]) -> Result<Vec<i16>, OpusError> {
        let n = self.inner.decode(packet, &mut self.buf, false)?;
        Ok(self.buf[..n].to_vec())
    }

    /// Decode one [`VoicePacket`].
    pub fn decode(&mut self, packet: &VoicePacket) -> Result<Vec<i16>, OpusError> {
        self.decode_packet(&packet.payload)
    }

    /// Packet-loss concealment for one missing frame. See the module docs.
    pub fn decode_lost(&mut self) -> Result<Vec<i16>, OpusError> {
        let n = self
            .inner
            .decode(&[], &mut self.buf[..SAMPLES_PER_FRAME], false)?;
        Ok(self.buf[..n].to_vec())
    }

    /// A whole capture, in one buffer.
    ///
    /// A packet libopus refuses is SKIPPED and the rest of the capture is still decoded — one
    /// corrupt 40-byte frame is 20 ms and losing the other twenty seconds with it is the wrong
    /// trade. The count of skipped packets comes back from [`Self::decode_all_counted`].
    pub fn decode_all(&mut self, packets: &[VoicePacket]) -> Result<Vec<i16>, OpusError> {
        Ok(self.decode_all_counted(packets).0)
    }

    /// [`Self::decode_all`], plus how many packets libopus refused.
    pub fn decode_all_counted(&mut self, packets: &[VoicePacket]) -> (Vec<i16>, usize) {
        let mut out = Vec::with_capacity(packets.len() * SAMPLES_PER_FRAME);
        let mut skipped = 0;
        for p in packets {
            match self.decode_packet(&p.payload) {
                Ok(pcm) => out.extend_from_slice(&pcm),
                Err(_) => skipped += 1,
            }
        }
        (out, skipped)
    }

    /// Forget the inter-frame state. Between two captures.
    pub fn reset(&mut self) -> Result<(), OpusError> {
        self.inner.reset_state()?;
        Ok(())
    }
}

/// A 16-bit mono PCM WAV file, in bytes. Std only — no dependency, no encoder.
///
/// Canonical 44-byte RIFF header: `RIFF` / `WAVE` / `fmt ` (PCM, 16-bit) / `data`. Every player
/// opens it.
pub fn wav_bytes(samples: &[i16], sample_rate_hz: u32) -> Result<Vec<u8>, OpusError> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .filter(|n| *n <= u32::MAX as usize - 36)
        .ok_or(OpusError::TooLong {
            samples: samples.len(),
        })?;
    let channels: u16 = 1;
    let bits: u16 = 16;
    let byte_rate = sample_rate_hz * channels as u32 * (bits / 8) as u32;
    let block_align = channels * (bits / 8);

    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // format 1 = PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    Ok(out)
}

/// Write PCM to a `.wav` at [`REQUESTED_SAMPLE_RATE_HZ`].
pub fn write_wav(path: &Path, samples: &[i16]) -> Result<(), OpusError> {
    write_wav_at(path, samples, REQUESTED_SAMPLE_RATE_HZ)
}

/// Write PCM to a `.wav` at an explicit rate.
pub fn write_wav_at(path: &Path, samples: &[i16], sample_rate_hz: u32) -> Result<(), OpusError> {
    let bytes = wav_bytes(samples, sample_rate_hz)?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(&bytes)?;
    Ok(())
}

/// Dump the raw Opus packets, one per line, as `<length> <hex>`.
///
/// A capture kept this way can be re-decoded later by any tool, which matters because the
/// decode is the lossy step and the packets are the evidence.
pub fn write_packet_dump(path: &Path, packets: &[VoicePacket]) -> Result<(), OpusError> {
    let mut f = std::fs::File::create(path)?;
    for p in packets {
        let hex: String = p.payload.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(f, "{} {}", p.payload.len(), hex)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WAV header is arithmetic and is checkable with no audio at all.
    #[test]
    fn the_wav_header_describes_sixteen_bit_mono_at_sixteen_kilohertz() {
        let pcm: Vec<i16> = (0..1600).map(|i| (i % 32767) as i16).collect();
        let w = wav_bytes(&pcm, REQUESTED_SAMPLE_RATE_HZ).unwrap();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(w[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(w[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(w[22..24].try_into().unwrap()), 1, "mono");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(w[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(w[34..36].try_into().unwrap()), 16);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 3200);
        assert_eq!(w.len(), 44 + 3200);
        assert_eq!(
            u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize,
            w.len() - 8
        );
    }

    #[test]
    fn one_second_of_audio_is_fifty_packets_of_three_hundred_and_twenty_samples() {
        assert_eq!(SAMPLES_PER_FRAME, 320);
        assert_eq!(SAMPLES_PER_FRAME * 50, REQUESTED_SAMPLE_RATE_HZ as usize);
        assert_eq!(MAX_SAMPLES_PER_PACKET, 1920, "120 ms, the Opus maximum");
    }

    /// A round trip through libopus with no device: encode silence, decode it back, check the
    /// geometry. Proves the link and the decoder configuration, not the glasses.
    #[test]
    fn a_twenty_millisecond_packet_decodes_to_one_frame_of_samples() {
        let mut enc = opus::Encoder::new(
            REQUESTED_SAMPLE_RATE_HZ,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .expect("an encoder");
        let silence = vec![0i16; SAMPLES_PER_FRAME];
        let packet = enc.encode_vec(&silence, 256).expect("a packet");

        let mut d = VoiceDecoder::new().expect("a decoder");
        let pcm = d.decode_packet(&packet).expect("decoded");
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
        assert_eq!(d.decode_lost().unwrap().len(), SAMPLES_PER_FRAME);
    }

    #[test]
    fn a_packet_libopus_refuses_costs_that_packet_and_not_the_capture() {
        let mut enc = opus::Encoder::new(
            REQUESTED_SAMPLE_RATE_HZ,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .unwrap();
        let good = enc.encode_vec(&vec![0i16; SAMPLES_PER_FRAME], 256).unwrap();
        let packets = vec![
            VoicePacket {
                payload: good.clone(),
                toc: None,
                index: 0,
            },
            VoicePacket {
                payload: Vec::new(), // empty: not a packet
                toc: None,
                index: 1,
            },
            VoicePacket {
                payload: good,
                toc: None,
                index: 2,
            },
        ];
        let mut d = VoiceDecoder::new().unwrap();
        let (pcm, skipped) = d.decode_all_counted(&packets);
        assert_eq!(skipped, 1);
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME * 2);
    }
}
