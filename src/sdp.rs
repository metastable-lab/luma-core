//! The DESCRIBE body — enough SDP to set up the glasses' live stream, and no more.
//!
//! The whole document the glasses return is 346 bytes and it does not change between sessions.
//! What a client needs out of it is four things per track: the payload type, the encoding, the
//! `control` suffix to put on the SETUP URL, and (for audio) the `fmtp` parameters that decide
//! how the RTP payload is framed.
//!
//! ```text
//! v=0
//! o=- 1 1 IN IP4 127.0.0.1
//! s=Test
//! a=type:broadcast
//! t=0 0
//! c=IN IP4 0.0.0.0
//! m=video 0 RTP/AVP 96
//! a=rtpmap:96 H264/90000
//! a=decode_buf=300
//! a=control:track0
//! m=audio 0 RTP/AVP 97
//! a=rtpmap:97 mpeg4-generic/16000/1
//! a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408; sizeLength=13; indexlength=3; indexdeltalength=3
//! a=control:track1
//! ```
//!
//! ## Two things about this document that trip clients up
//!
//! **`fmtp` keys are not spelled the way RFC 3640 spells them.** The RFC says `indexLength` and
//! `indexDeltaLength`; this server sends `indexlength` and `indexdeltalength`. A case-sensitive
//! lookup finds `sizeLength` and misses the other two, and a client that then falls back to
//! zero-width index fields mis-frames every audio packet. [`Fmtp::get`] is
//! case-insensitive for exactly this reason, and there is a test on it.
//!
//! **`a=decode_buf=300` is an attribute with no colon.** It is `a=<flag>` shaped, where the
//! flag happens to contain an `=`. A parser that splits every attribute on `:` and requires a
//! value drops it; one that splits on `=` instead mangles `a=control:track0`. Attributes are
//! kept as `(name, Option<value>)` and split on the FIRST colon only.
//!
//! **`m=` port 0 does not mean "disabled" here.** Both media lines carry port 0 because the
//! transport is negotiated per track in SETUP. The client's own port is what matters; see
//! [`crate::rtsp`].

use core::fmt;

/// What kind of stream a media section describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    /// Anything else, kept verbatim. Nothing has ever sent one here.
    Other(String),
}

impl MediaKind {
    fn parse(s: &str) -> MediaKind {
        match s {
            "video" => MediaKind::Video,
            "audio" => MediaKind::Audio,
            other => MediaKind::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Other(s) => s,
        }
    }
}

/// One `a=rtpmap:<pt> <encoding>/<clock>[/<channels>]` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMap {
    pub payload_type: u8,
    /// `H264`, `mpeg4-generic` — verbatim, case as sent.
    pub encoding: String,
    /// 90000 for the video track, 16000 for audio. Note the audio clock rate IS the sample
    /// rate here, which is what an RTP timestamp on that track counts in.
    pub clock_rate: u32,
    /// Present on the audio line (`/1`), absent on video.
    pub channels: Option<u8>,
}

/// One `a=fmtp:<pt> k=v; k=v; …` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fmtp {
    pub payload_type: u8,
    /// Key/value pairs in the order sent, keys as spelled on the wire.
    pub params: Vec<(String, String)>,
}

impl Fmtp {
    /// Look up a parameter, CASE-INSENSITIVELY. See the module docs — the server's spelling of
    /// `indexlength` disagrees with RFC 3640's and a case-sensitive lookup silently returns
    /// `None`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// A parameter parsed as an integer.
    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key)?.trim().parse().ok()
    }

    /// `config=1408` — the AudioSpecificConfig, as hex. Feed it to
    /// [`crate::aac::parse_config_hex`].
    pub fn config(&self) -> Option<&str> {
        self.get("config")
    }

    /// `mode=AAC-hbr`. The only mode this server offers, and the one
    /// [`crate::aac`] implements.
    pub fn mode(&self) -> Option<&str> {
        self.get("mode")
    }

    /// `sizeLength` — bits of AU size per AU header. 13 here.
    pub fn size_length(&self) -> Option<u32> {
        self.get_u32("sizeLength")
    }

    /// `indexLength` — bits of AU index in the FIRST header. 3 here, spelled `indexlength`.
    pub fn index_length(&self) -> Option<u32> {
        self.get_u32("indexLength")
    }

    /// `indexDeltaLength` — bits of AU index delta in SUBSEQUENT headers. 3 here, spelled
    /// `indexdeltalength`.
    pub fn index_delta_length(&self) -> Option<u32> {
        self.get_u32("indexDeltaLength")
    }

    /// `sprop-parameter-sets` — base64 SPS/PPS, comma separated.
    ///
    /// This server does NOT send it: it puts SPS and PPS in-band as a STAP-A ahead of every
    /// IDR (§14), which is the more robust arrangement because a decoder that joins late still
    /// gets them. Parsed anyway, because a client that only ever reads parameter sets from SDP
    /// works against most other cameras and fails against this one in a way that looks like a
    /// decoder bug.
    pub fn sprop_parameter_sets(&self) -> Option<&str> {
        self.get("sprop-parameter-sets")
    }

    /// The SPS/PPS NAL units from `sprop-parameter-sets`, base64-decoded.
    ///
    /// Empty when the attribute is absent, which is the normal case here.
    pub fn sprop_nal_units(&self) -> Vec<Vec<u8>> {
        self.sprop_parameter_sets()
            .map(|s| {
                s.split(',')
                    .filter_map(|p| base64_decode(p.trim()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One `m=` section and the attributes under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    pub kind: MediaKind,
    /// The `m=` port. `0` on both tracks here — see the module docs.
    pub port: u16,
    /// `RTP/AVP`.
    pub protocol: String,
    /// The format list from the `m=` line. One entry on both tracks here.
    pub formats: Vec<u8>,
    /// The first format, which is the dynamic payload type to expect on the wire: 96 video,
    /// 97 audio.
    pub payload_type: u8,
    pub rtpmap: Option<RtpMap>,
    pub fmtp: Option<Fmtp>,
    /// `a=control:track0`. Appended to the base URL to make the SETUP URL.
    pub control: Option<String>,
    /// Every `a=` line in this section, `(name, value)`, split on the first colon.
    pub attributes: Vec<(String, Option<String>)>,
}

impl MediaDescription {
    /// The URL to SETUP this track against, given the stream's base URL.
    ///
    /// An absolute `control` (`rtsp://…`) replaces the base; a relative one is appended with a
    /// single separating slash. `*` — the "aggregate" control — means the base itself.
    pub fn setup_url(&self, base_url: &str) -> String {
        match self.control.as_deref() {
            None | Some("*") => base_url.to_string(),
            Some(c) if c.contains("://") => c.to_string(),
            Some(c) => format!(
                "{}/{}",
                base_url.trim_end_matches('/'),
                c.trim_start_matches('/')
            ),
        }
    }

    /// Whether this section describes H.264 — by rtpmap encoding, not by payload type, because
    /// 96 is dynamic and means nothing on its own.
    pub fn is_h264(&self) -> bool {
        self.rtpmap
            .as_ref()
            .is_some_and(|m| m.encoding.eq_ignore_ascii_case("H264"))
    }

    /// Whether this section describes MPEG-4 audio (`mpeg4-generic`, i.e. AAC here).
    pub fn is_mpeg4_audio(&self) -> bool {
        self.rtpmap
            .as_ref()
            .is_some_and(|m| m.encoding.eq_ignore_ascii_case("mpeg4-generic"))
    }
}

/// A parsed session description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionDescription {
    /// `v=`.
    pub version: Option<String>,
    /// `o=`.
    pub origin: Option<String>,
    /// `s=`. A session name, not user-facing copy — this server sends `Test`.
    pub name: Option<String>,
    /// `c=` at session level.
    pub connection: Option<String>,
    /// Session-level `a=` lines, before the first `m=`.
    pub attributes: Vec<(String, Option<String>)>,
    pub media: Vec<MediaDescription>,
}

impl SessionDescription {
    /// The first video section, if any.
    pub fn video(&self) -> Option<&MediaDescription> {
        self.media.iter().find(|m| m.kind == MediaKind::Video)
    }

    /// The first audio section, if any.
    pub fn audio(&self) -> Option<&MediaDescription> {
        self.media.iter().find(|m| m.kind == MediaKind::Audio)
    }

    /// A session-level attribute, case-insensitively.
    pub fn attribute(&self, name: &str) -> Option<Option<&str>> {
        self.attributes
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_deref())
    }
}

/// Why a body is not a session description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdpError {
    /// A line was not `<one letter>=<value>`.
    BadLine { line: String },
    /// An `m=` line did not have `<media> <port> <proto> <fmt>…`.
    BadMediaLine { line: String },
    /// An `a=rtpmap:` line did not have `<pt> <encoding>/<clock>`.
    BadRtpMap { line: String },
    /// A payload type or port was outside its range.
    BadNumber { line: String },
    /// No `m=` section at all. A description with no media is not one a client can use.
    NoMedia,
}

impl fmt::Display for SdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SdpError::BadLine { line } => write!(f, "not an SDP line: {line:?}"),
            SdpError::BadMediaLine { line } => write!(f, "bad m= line: {line:?}"),
            SdpError::BadRtpMap { line } => write!(f, "bad rtpmap: {line:?}"),
            SdpError::BadNumber { line } => write!(f, "bad number in: {line:?}"),
            SdpError::NoMedia => write!(f, "no m= section"),
        }
    }
}

impl std::error::Error for SdpError {}

/// Parse a DESCRIBE body.
///
/// Tolerant where tolerance is safe: bare `\n` line endings as well as `\r\n`, blank lines
/// ignored, unknown `<letter>=` lines ignored, unknown attributes kept. Strict where it is not:
/// a malformed `m=` or `rtpmap` is an error rather than a silently dropped track, because a
/// client that "successfully" parses a description with no video track reports the camera as
/// having no camera.
pub fn parse(text: &str) -> Result<SessionDescription, SdpError> {
    let mut sd = SessionDescription::default();
    let mut current: Option<MediaDescription> = None;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let mut it = line.splitn(2, '=');
        let key = it.next().unwrap_or_default();
        let value = match it.next() {
            Some(v) => v,
            None => {
                return Err(SdpError::BadLine {
                    line: line.to_string(),
                })
            }
        };
        if key.len() != 1 {
            return Err(SdpError::BadLine {
                line: line.to_string(),
            });
        }

        match key.as_bytes()[0] {
            b'v' => sd.version = Some(value.to_string()),
            b'o' => sd.origin = Some(value.to_string()),
            b's' => sd.name = Some(value.to_string()),
            b'c' => {
                if let Some(m) = current.as_mut() {
                    m.attributes
                        .push(("c".to_string(), Some(value.to_string())));
                } else {
                    sd.connection = Some(value.to_string());
                }
            }
            b'm' => {
                if let Some(m) = current.take() {
                    sd.media.push(m);
                }
                current = Some(parse_media_line(line, value)?);
            }
            b'a' => {
                let (name, val) = split_attribute(value);
                match current.as_mut() {
                    Some(m) => apply_media_attribute(m, line, &name, val.as_deref())?,
                    None => sd.attributes.push((name, val)),
                }
            }
            // t=, b=, i=, u=, e=, p=, k=, z=, r= — recorded nowhere because nothing here reads
            // them, and ignoring an unknown line is what lets a firmware add one safely.
            _ => {}
        }
    }
    if let Some(m) = current.take() {
        sd.media.push(m);
    }
    if sd.media.is_empty() {
        return Err(SdpError::NoMedia);
    }
    Ok(sd)
}

fn parse_media_line(line: &str, value: &str) -> Result<MediaDescription, SdpError> {
    let mut parts = value.split_whitespace();
    let media = parts.next().ok_or_else(|| SdpError::BadMediaLine {
        line: line.to_string(),
    })?;
    let port_field = parts.next().ok_or_else(|| SdpError::BadMediaLine {
        line: line.to_string(),
    })?;
    // `m=video 0/2 RTP/AVP 96` is legal SDP; the port count after the slash is not used here.
    let port: u16 = port_field
        .split('/')
        .next()
        .unwrap_or(port_field)
        .parse()
        .map_err(|_| SdpError::BadNumber {
            line: line.to_string(),
        })?;
    let protocol = parts
        .next()
        .ok_or_else(|| SdpError::BadMediaLine {
            line: line.to_string(),
        })?
        .to_string();
    let mut formats = Vec::new();
    for f in parts {
        formats.push(f.parse::<u8>().map_err(|_| SdpError::BadNumber {
            line: line.to_string(),
        })?);
    }
    let payload_type = *formats.first().ok_or_else(|| SdpError::BadMediaLine {
        line: line.to_string(),
    })?;
    Ok(MediaDescription {
        kind: MediaKind::parse(media),
        port,
        protocol,
        formats,
        payload_type,
        rtpmap: None,
        fmtp: None,
        control: None,
        attributes: Vec::new(),
    })
}

/// Split `a=` content on the FIRST colon. `decode_buf=300` has no colon and stays whole.
fn split_attribute(value: &str) -> (String, Option<String>) {
    match value.split_once(':') {
        Some((k, v)) => (k.to_string(), Some(v.to_string())),
        None => (value.to_string(), None),
    }
}

fn apply_media_attribute(
    m: &mut MediaDescription,
    line: &str,
    name: &str,
    value: Option<&str>,
) -> Result<(), SdpError> {
    match (name, value) {
        ("rtpmap", Some(v)) => m.rtpmap = Some(parse_rtpmap(line, v)?),
        ("fmtp", Some(v)) => m.fmtp = Some(parse_fmtp(line, v)?),
        ("control", Some(v)) => m.control = Some(v.trim().to_string()),
        _ => {}
    }
    m.attributes
        .push((name.to_string(), value.map(str::to_string)));
    Ok(())
}

fn parse_rtpmap(line: &str, value: &str) -> Result<RtpMap, SdpError> {
    let (pt, rest) = value
        .trim()
        .split_once(' ')
        .ok_or_else(|| SdpError::BadRtpMap {
            line: line.to_string(),
        })?;
    let payload_type = pt.parse::<u8>().map_err(|_| SdpError::BadNumber {
        line: line.to_string(),
    })?;
    let mut fields = rest.trim().split('/');
    let encoding = fields
        .next()
        .ok_or_else(|| SdpError::BadRtpMap {
            line: line.to_string(),
        })?
        .to_string();
    let clock_rate = fields
        .next()
        .ok_or_else(|| SdpError::BadRtpMap {
            line: line.to_string(),
        })?
        .parse::<u32>()
        .map_err(|_| SdpError::BadNumber {
            line: line.to_string(),
        })?;
    let channels = match fields.next() {
        Some(c) => Some(c.parse::<u8>().map_err(|_| SdpError::BadNumber {
            line: line.to_string(),
        })?),
        None => None,
    };
    Ok(RtpMap {
        payload_type,
        encoding,
        clock_rate,
        channels,
    })
}

fn parse_fmtp(line: &str, value: &str) -> Result<Fmtp, SdpError> {
    let (pt, rest) = value
        .trim()
        .split_once(' ')
        .ok_or_else(|| SdpError::BadLine {
            line: line.to_string(),
        })?;
    let payload_type = pt.parse::<u8>().map_err(|_| SdpError::BadNumber {
        line: line.to_string(),
    })?;
    let mut params = Vec::new();
    for pair in rest.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => params.push((k.trim().to_string(), v.trim().to_string())),
            // A bare flag with no `=`. Kept with an empty value rather than dropped.
            None => params.push((pair.to_string(), String::new())),
        }
    }
    Ok(Fmtp {
        payload_type,
        params,
    })
}

/// Standard base64 decode, no padding requirement. Small enough to keep the crate dependency-free.
///
/// Returns `None` on any character outside the alphabet, which is what tells a caller that a
/// `sprop-parameter-sets` value is not what it claimed to be.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DESCRIBE body verbatim, cut from a live session with the glasses. 346 bytes, which
    /// is exactly the `Content-Length` the server declared.
    pub(crate) const LIVE_SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 127.0.0.1\r\n",
        "s=Test\r\n",
        "a=type:broadcast\r\n",
        "t=0 0\r\n",
        "c=IN IP4 0.0.0.0\r\n",
        "m=video 0 RTP/AVP 96\r\n",
        "a=rtpmap:96 H264/90000\r\n",
        "a=decode_buf=300\r\n",
        "a=control:track0\r\n",
        "m=audio 0 RTP/AVP 97\r\n",
        "a=rtpmap:97 mpeg4-generic/16000/1\r\n",
        "a=fmtp:97 profile-level-id=1; mode=AAC-hbr; config=1408; sizeLength=13; ",
        "indexlength=3; indexdeltalength=3\r\n",
        "a=control:track1\r\n",
    );

    #[test]
    fn the_live_describe_body_yields_a_video_track_and_an_audio_track() {
        let sd = parse(LIVE_SDP).expect("an SDP document");
        assert_eq!(sd.version.as_deref(), Some("0"));
        assert_eq!(sd.name.as_deref(), Some("Test"));
        assert_eq!(sd.connection.as_deref(), Some("IN IP4 0.0.0.0"));
        assert_eq!(sd.attribute("type"), Some(Some("broadcast")));
        assert_eq!(sd.media.len(), 2);

        let v = sd.video().expect("a video track");
        assert_eq!(v.payload_type, 96);
        assert_eq!(v.port, 0, "the transport is negotiated in SETUP, not here");
        assert_eq!(v.protocol, "RTP/AVP");
        assert!(v.is_h264());
        assert_eq!(
            v.rtpmap,
            Some(RtpMap {
                payload_type: 96,
                encoding: "H264".into(),
                clock_rate: 90_000,
                channels: None
            })
        );
        assert_eq!(v.control.as_deref(), Some("track0"));
        assert_eq!(v.fmtp, None, "no sprop-parameter-sets: SPS/PPS are in-band");

        let a = sd.audio().expect("an audio track");
        assert_eq!(a.payload_type, 97);
        assert!(a.is_mpeg4_audio());
        assert_eq!(
            a.rtpmap,
            Some(RtpMap {
                payload_type: 97,
                encoding: "mpeg4-generic".into(),
                clock_rate: 16_000,
                channels: Some(1)
            })
        );
        assert_eq!(a.control.as_deref(), Some("track1"));
    }

    /// The bug this parser exists to not have: RFC 3640 spells these `indexLength` and
    /// `indexDeltaLength`, and the server does not.
    #[test]
    fn the_fmtp_lookup_is_case_insensitive_because_the_server_lowercases_two_keys() {
        let sd = parse(LIVE_SDP).unwrap();
        let f = sd.audio().unwrap().fmtp.clone().expect("an fmtp line");
        assert_eq!(f.payload_type, 97);
        assert_eq!(f.mode(), Some("AAC-hbr"));
        assert_eq!(f.config(), Some("1408"));
        assert_eq!(f.size_length(), Some(13));
        assert_eq!(
            f.index_length(),
            Some(3),
            "spelled `indexlength` on the wire"
        );
        assert_eq!(
            f.index_delta_length(),
            Some(3),
            "spelled `indexdeltalength` on the wire"
        );
        assert_eq!(f.get("PROFILE-LEVEL-ID"), Some("1"));
        // The keys are kept as sent, so a client echoing them back sends what the server said.
        assert!(f.params.iter().any(|(k, _)| k == "indexlength"));
    }

    #[test]
    fn an_attribute_with_an_equals_and_no_colon_survives_whole() {
        let sd = parse(LIVE_SDP).unwrap();
        let v = sd.video().unwrap();
        assert!(
            v.attributes
                .iter()
                .any(|(k, val)| k == "decode_buf=300" && val.is_none()),
            "a=decode_buf=300 is a flag whose name contains an ="
        );
    }

    #[test]
    fn the_control_suffix_becomes_the_setup_url() {
        let sd = parse(LIVE_SDP).unwrap();
        let base = "rtsp://192.168.169.1:554/h264";
        assert_eq!(
            sd.video().unwrap().setup_url(base),
            "rtsp://192.168.169.1:554/h264/track0"
        );
        assert_eq!(
            sd.audio().unwrap().setup_url(base),
            "rtsp://192.168.169.1:554/h264/track1"
        );
    }

    #[test]
    fn an_absolute_or_aggregate_control_replaces_the_base_rather_than_appending_to_it() {
        let mut m = parse(LIVE_SDP).unwrap().media.remove(0);
        m.control = Some("rtsp://10.0.0.1/other/track9".into());
        assert_eq!(m.setup_url("rtsp://x/y"), "rtsp://10.0.0.1/other/track9");
        m.control = Some("*".into());
        assert_eq!(m.setup_url("rtsp://x/y"), "rtsp://x/y");
        m.control = None;
        assert_eq!(m.setup_url("rtsp://x/y"), "rtsp://x/y");
    }

    #[test]
    fn bare_newlines_parse_the_same_as_crlf() {
        let unix = LIVE_SDP.replace("\r\n", "\n");
        assert_eq!(parse(&unix).unwrap(), parse(LIVE_SDP).unwrap());
    }

    #[test]
    fn a_body_that_is_not_sdp_is_rejected_rather_than_yielding_an_empty_session() {
        assert!(matches!(parse("<html>"), Err(SdpError::BadLine { .. })));
        assert_eq!(parse("v=0\r\ns=Test\r\n"), Err(SdpError::NoMedia));
        assert!(matches!(
            parse("v=0\r\nm=video\r\n"),
            Err(SdpError::BadMediaLine { .. })
        ));
        assert!(matches!(
            parse("v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264\r\n"),
            Err(SdpError::BadRtpMap { .. })
        ));
        assert!(matches!(
            parse("v=0\r\nm=video 0 RTP/AVP 999\r\n"),
            Err(SdpError::BadNumber { .. })
        ));
    }

    #[test]
    fn sprop_parameter_sets_are_decoded_when_a_server_does_send_them() {
        // Not this server — a synthetic line in the RFC 6184 shape, to pin the decoder. The
        // two payloads are the SPS and PPS this camera sends in-band instead.
        let sdp = concat!(
            "v=0\r\nm=video 0 RTP/AVP 96\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z00ANJZUAyAS9IgI,aO44gA==\r\n",
        );
        let sd = parse(sdp).unwrap();
        let f = sd.video().unwrap().fmtp.clone().unwrap();
        let nals = f.sprop_nal_units();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0][0] & 0x1F, 7, "SPS");
        assert_eq!(nals[1][0] & 0x1F, 8, "PPS");
        assert_eq!(
            nals[0],
            vec![0x67, 0x4D, 0x00, 0x34, 0x96, 0x54, 0x03, 0x20, 0x12, 0xF4, 0x88, 0x08]
        );
        assert_eq!(nals[1], vec![0x68, 0xEE, 0x38, 0x80]);
    }

    #[test]
    fn base64_rejects_a_value_outside_its_alphabet() {
        assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
        assert_eq!(
            base64_decode("aGk"),
            Some(b"hi".to_vec()),
            "padding optional"
        );
        assert_eq!(base64_decode("not base64!"), None);
    }
}
