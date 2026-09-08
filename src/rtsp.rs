//! RTSP, message layer only — the OPTIONS → DESCRIBE → SETUP → PLAY conversation as a state
//! machine over bytes.
//!
//! After `0x67` raises the SoftAP (§12) the live stream is a plain RTSP server on port 554.
//! There is no HTTP handshake, no authentication and no BLE traffic in between: connect a TCP
//! socket and start talking.
//!
//! ```text
//! rtsp://192.168.169.1:554/h264      Server: Hisilicon RTSP Streaming Media Server/1.0.0
//! ```
//!
//! [`RtspSession`] owns the conversation and NOT the socket. [`RtspSession::next_request`]
//! hands you the exact bytes to write; [`RtspSession::feed`] takes whatever came back, in
//! whatever chunks it arrived in, and tells you what it meant. That split is the same one the
//! rest of this crate makes, and it is what lets the whole exchange be tested against a
//! transcript with no network in the room.
//!
//! ## The exchange, as the glasses actually run it
//!
//! ```text
//! → OPTIONS rtsp://192.168.169.1:554/h264 RTSP/1.0    CSeq: 1
//! ← RTSP/1.0 200 OK   Public: DESCRIBE, SETUP, TEARDOWN, PLAY, PAUSE
//! → DESCRIBE …                                        CSeq: 2   Accept: application/sdp
//! ← RTSP/1.0 200 OK   Content-Length: 346             (the SDP, §14)
//! → SETUP  …/track0   Transport: RTP/AVP/UDP;unicast;client_port=8712-8713
//! ← RTSP/1.0 200 OK   Transport: …;server_port=53796-53797
//!                     Session: 82838485868788898A8B8C8D8E8F90
//! → PLAY   …          Range: npt=0.000-               Session: …
//! ← RTSP/1.0 200 OK
//! ```
//!
//! Three things about this server are worth knowing before writing a client for it:
//!
//! * **`Session` has no timeout parameter.** Most servers answer SETUP with
//!   `Session: <id>;timeout=60`; this one sends the id alone. There is nothing to refresh
//!   against, so a client that schedules keep-alive `OPTIONS` from a parsed timeout schedules
//!   them from a default it invented.
//! * **The session id is a 30-character hex string** and it is the SAME value in every capture
//!   (`82838485868788898A8B8C8D8E8F90`, a byte ramp). Do not treat it as a nonce and do not
//!   assume it is unique per connection.
//! * **PLAY is addressed to the base URL, SETUP to the track URL.** Sending PLAY to
//!   `…/h264/track0` is a different request; the base URL is what the capture uses.
//!
//! ## Audio is optional and must stay optional
//!
//! The SDP advertises `track1` (AAC-LC 16 kHz mono). Every live session captured set up
//! `track0` only, so the audio path is exercised by no capture. [`RtspSession::request_audio`]
//! turns the second SETUP on; a failure there is reported as an event and does NOT stop the
//! video, which is the §14 rule stated in code.
//!
//! ## Not handled here
//!
//! Interleaved (RTP-over-TCP, `$`-prefixed) transport. The server offers UDP, both captures use
//! UDP, and a `$` arriving on this connection is reported as
//! [`RtspError::InterleavedNotSupported`] rather than silently consumed as garbage.

use core::fmt;

use crate::sdp::{self, SessionDescription};

/// The RTSP port. Fixed in firmware.
pub const PORT: u16 = 554;

/// The stream path. One stream, always this name.
pub const PATH: &str = "/h264";

/// `rtsp://192.168.169.1:554/h264` — the whole URL, for the default host.
pub const URL: &str = "rtsp://192.168.169.1:554/h264";

/// The `User-Agent` this crate sends. Identifies the client in the server's log; the server does
/// not branch on it.
pub const USER_AGENT: &str = "luma-core";

/// Default client ports for the video track — the pair PROTOCOL.md §14 quotes.
///
/// RTP on the even port, RTCP on the odd one; RFC 3550 requires them consecutive with RTP even.
pub const DEFAULT_VIDEO_CLIENT_PORTS: (u16, u16) = (8712, 8713);

/// Default client ports for the audio track. The next free pair up.
pub const DEFAULT_AUDIO_CLIENT_PORTS: (u16, u16) = (8714, 8715);

/// The RTSP URL for a host that is not the default.
pub fn stream_url(host: &str) -> String {
    format!("rtsp://{host}:{PORT}{PATH}")
}

/// Which track a SETUP is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Track {
    Video,
    Audio,
}

/// Where the conversation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    /// Nothing sent yet.
    Init,
    /// OPTIONS is out, waiting.
    AwaitingOptions,
    /// DESCRIBE is out, waiting.
    AwaitingDescribe,
    /// A SETUP is out, waiting.
    AwaitingSetup(Track),
    /// PLAY is out, waiting.
    AwaitingPlay,
    /// The server is streaming.
    Playing,
    /// TEARDOWN is out.
    AwaitingTeardown,
    /// Finished, cleanly or otherwise.
    Closed,
}

/// One parsed RTSP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    /// The `CSeq` header. Absent is a protocol violation this parser reports rather than
    /// tolerates — it is the only thing tying a response to a request.
    pub cseq: Option<u32>,
    /// Headers in the order received, names as sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A header by name, case-insensitively — RTSP header names are case-insensitive and this
    /// server capitalises them differently from most.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, for the SDP.
    pub fn body_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.body).ok()
    }
}

/// A parsed `Transport` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transport {
    /// `RTP/AVP` or `RTP/AVP/UDP`.
    pub protocol: String,
    pub unicast: bool,
    /// The ports we asked for, echoed back.
    pub client_ports: Option<(u16, u16)>,
    /// The ports the server will send FROM. Useful for a client that firewalls its socket to
    /// one peer, and for nothing else — the data arrives at `client_ports` either way.
    pub server_ports: Option<(u16, u16)>,
    /// `ssrc=…` if the server declares one. This one does not.
    pub ssrc: Option<u32>,
    /// The header verbatim, so nothing is lost.
    pub raw: String,
}

/// Parse a `Transport` header value.
pub fn parse_transport(value: &str) -> Transport {
    let mut t = Transport {
        raw: value.to_string(),
        ..Transport::default()
    };
    for (i, part) in value.split(';').enumerate() {
        let part = part.trim();
        if i == 0 {
            t.protocol = part.to_string();
            continue;
        }
        if part.eq_ignore_ascii_case("unicast") {
            t.unicast = true;
            continue;
        }
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "client_port" => t.client_ports = parse_port_pair(v),
            "server_port" => t.server_ports = parse_port_pair(v),
            "ssrc" => t.ssrc = u32::from_str_radix(v.trim(), 16).ok(),
            _ => {}
        }
    }
    t
}

fn parse_port_pair(v: &str) -> Option<(u16, u16)> {
    let v = v.trim();
    match v.split_once('-') {
        Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
        // A single port is legal; RTCP is then implicitly the next one up.
        None => {
            let a: u16 = v.parse().ok()?;
            Some((a, a.checked_add(1)?))
        }
    }
}

/// What a fed chunk of bytes meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server answered OPTIONS. `methods` is the `Public` header, split.
    Options { methods: Vec<String> },
    /// The DESCRIBE body parsed. This is where the payload types and the `control` suffixes
    /// come from.
    Described(Box<SessionDescription>),
    /// A track is set up. `server_ports` is from the `Transport` header.
    SetUp {
        track: Track,
        server_ports: Option<(u16, u16)>,
        session_id: String,
    },
    /// A SETUP was refused. Only ever emitted for [`Track::Audio`] — a refused video SETUP is
    /// an error, because there is then nothing to play.
    SetUpRefused { track: Track, status: u16 },
    /// PLAY succeeded; RTP is on its way to the client ports.
    Playing,
    /// TEARDOWN was acknowledged.
    TornDown,
    /// A response that did not advance the state machine, handed over rather than dropped.
    Other(Box<Response>),
}

/// Why the conversation cannot continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    /// The first line was not `RTSP/1.x <code> <reason>`.
    BadStatusLine { line: String },
    /// A header line had no colon.
    BadHeader { line: String },
    /// `Content-Length` was present and not a number.
    BadContentLength { value: String },
    /// The response's `CSeq` did not match the request in flight. Fatal: it means the responses
    /// and requests have desynchronised and every later match would be wrong too.
    CseqMismatch { expected: u32, got: Option<u32> },
    /// A non-2xx status for a request the conversation needs.
    Status {
        status: u16,
        reason: String,
        cseq: Option<u32>,
    },
    /// DESCRIBE succeeded and its body is not SDP.
    BadSdp(sdp::SdpError),
    /// DESCRIBE succeeded and its SDP has no video track.
    NoVideoTrack,
    /// SETUP succeeded with no `Session` header, so there is nothing to put on PLAY.
    NoSessionHeader,
    /// A `$` byte where a response was expected: the server switched to interleaved transport.
    InterleavedNotSupported,
    /// A response arrived with no request outstanding.
    Unsolicited,
}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtspError::BadStatusLine { line } => write!(f, "bad status line: {line:?}"),
            RtspError::BadHeader { line } => write!(f, "bad header: {line:?}"),
            RtspError::BadContentLength { value } => write!(f, "bad Content-Length: {value:?}"),
            RtspError::CseqMismatch { expected, got } => {
                write!(f, "CSeq {got:?}, expected {expected}")
            }
            RtspError::Status { status, reason, .. } => write!(f, "server said {status} {reason}"),
            RtspError::BadSdp(e) => write!(f, "bad SDP: {e}"),
            RtspError::NoVideoTrack => write!(f, "the description has no video track"),
            RtspError::NoSessionHeader => write!(f, "SETUP returned no Session header"),
            RtspError::InterleavedNotSupported => write!(f, "interleaved RTP is not supported"),
            RtspError::Unsolicited => write!(f, "a response with no request outstanding"),
        }
    }
}

impl std::error::Error for RtspError {}

/// The OPTIONS → DESCRIBE → SETUP → PLAY conversation, as a state machine over bytes.
///
/// Sans-IO: it produces request bytes and consumes response bytes. It opens nothing, waits for
/// nothing and has no clock — a client that wants a response timeout keeps its own.
#[derive(Debug, Clone)]
pub struct RtspSession {
    url: String,
    user_agent: String,
    video_ports: (u16, u16),
    audio_ports: (u16, u16),
    want_audio: bool,
    state: State,
    cseq: u32,
    /// The CSeq of the request currently outstanding, if any.
    pending: Option<u32>,
    session_id: Option<String>,
    sdp: Option<SessionDescription>,
    video_server_ports: Option<(u16, u16)>,
    audio_server_ports: Option<(u16, u16)>,
    audio_set_up: bool,
    buffer: Vec<u8>,
}

impl RtspSession {
    /// A session against the glasses at their default address.
    pub fn new() -> Self {
        RtspSession::for_url(URL)
    }

    /// A session against an arbitrary RTSP URL.
    pub fn for_url(url: &str) -> Self {
        Self {
            url: url.to_string(),
            user_agent: USER_AGENT.to_string(),
            video_ports: DEFAULT_VIDEO_CLIENT_PORTS,
            audio_ports: DEFAULT_AUDIO_CLIENT_PORTS,
            want_audio: false,
            state: State::Init,
            cseq: 0,
            pending: None,
            session_id: None,
            sdp: None,
            video_server_ports: None,
            audio_server_ports: None,
            audio_set_up: false,
            buffer: Vec::new(),
        }
    }

    /// Where the client will listen for video RTP/RTCP. RTP must be the even port.
    pub fn video_client_ports(mut self, ports: (u16, u16)) -> Self {
        self.video_ports = ports;
        self
    }

    /// Where the client will listen for audio RTP/RTCP.
    pub fn audio_client_ports(mut self, ports: (u16, u16)) -> Self {
        self.audio_ports = ports;
        self
    }

    /// Also SETUP the audio track. Off by default: no capture exercises it, and a refusal must
    /// never cost you the video.
    pub fn request_audio(mut self, want: bool) -> Self {
        self.want_audio = want;
        self
    }

    /// Override the `User-Agent`.
    pub fn user_agent(mut self, agent: &str) -> Self {
        self.user_agent = agent.to_string();
        self
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The parsed DESCRIBE body, once it has arrived.
    pub fn sdp(&self) -> Option<&SessionDescription> {
        self.sdp.as_ref()
    }

    /// The ports the server streams video FROM, from the SETUP `Transport` header.
    pub fn video_server_ports(&self) -> Option<(u16, u16)> {
        self.video_server_ports
    }

    /// The ports the server streams audio FROM, when a track1 SETUP succeeded.
    pub fn audio_server_ports(&self) -> Option<(u16, u16)> {
        self.audio_server_ports
    }

    /// The client ports the caller configured, for binding the sockets.
    pub fn client_ports(&self, track: Track) -> (u16, u16) {
        match track {
            Track::Video => self.video_ports,
            Track::Audio => self.audio_ports,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state == State::Playing
    }

    /// The CSeq of the request awaiting a response.
    pub fn pending_cseq(&self) -> Option<u32> {
        self.pending
    }

    /// The next request to write, or `None` when there is nothing to send — either a response is
    /// outstanding, or the stream is playing, or the session is closed.
    ///
    /// One request is in flight at a time. RTSP allows pipelining; this server was never seen
    /// pipelined to and there is nothing to gain from four round trips becoming three.
    pub fn next_request(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_some() {
            return None;
        }
        match self.state {
            State::Init => {
                self.state = State::AwaitingOptions;
                Some(self.request("OPTIONS", &self.url.clone(), &[]))
            }
            State::AwaitingOptions
            | State::AwaitingDescribe
            | State::AwaitingSetup(_)
            | State::AwaitingPlay
            | State::AwaitingTeardown
            | State::Playing
            | State::Closed => None,
        }
    }

    /// Feed bytes from the socket. Returns everything they completed.
    ///
    /// Partial reads are fine — the buffer keeps a half-arrived message and the next call
    /// finishes it. Several responses in one read are fine too.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, RtspError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.buffer.first() == Some(&b'$') {
                return Err(RtspError::InterleavedNotSupported);
            }
            match take_response(&mut self.buffer)? {
                Some(resp) => match self.handle(resp) {
                    Ok(Some(e)) => events.push(e),
                    Ok(None) => {}
                    Err(e) => return Err(e),
                },
                None => return Ok(events),
            }
        }
    }

    /// The TEARDOWN request. Also moves the state machine to [`State::AwaitingTeardown`] so a
    /// caller can drive it to a clean close.
    ///
    /// Safe to skip: dropping the TCP connection ends the session too, and the `0x44` BLE
    /// teardown (§12) is what actually powers the radio down. Sending it is politer and lets the
    /// server free its socket immediately.
    pub fn teardown(&mut self) -> Vec<u8> {
        let url = self.url.clone();
        let bytes = self.request("TEARDOWN", &url, &[]);
        self.state = State::AwaitingTeardown;
        bytes
    }

    /// Give up on the conversation without sending anything.
    pub fn close(&mut self) {
        self.state = State::Closed;
        self.pending = None;
        self.buffer.clear();
    }

    // -- request construction ------------------------------------------------------------

    fn request(&mut self, method: &str, url: &str, extra: &[(&str, String)]) -> Vec<u8> {
        self.cseq += 1;
        self.pending = Some(self.cseq);
        let mut s = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (k, v) in extra {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str(&format!("User-Agent: {}\r\n", self.user_agent));
        if let Some(id) = &self.session_id {
            s.push_str(&format!("Session: {id}\r\n"));
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    fn describe(&mut self) -> Vec<u8> {
        let url = self.url.clone();
        self.state = State::AwaitingDescribe;
        self.request(
            "DESCRIBE",
            &url,
            &[("Accept", "application/sdp".to_string())],
        )
    }

    fn setup(&mut self, track: Track) -> Vec<u8> {
        let (lo, hi) = self.client_ports(track);
        let control_url = self
            .sdp
            .as_ref()
            .and_then(|s| match track {
                Track::Video => s.video(),
                Track::Audio => s.audio(),
            })
            .map(|m| m.setup_url(&self.url))
            .unwrap_or_else(|| self.url.clone());
        self.state = State::AwaitingSetup(track);
        self.request(
            "SETUP",
            &control_url,
            &[(
                "Transport",
                format!("RTP/AVP/UDP;unicast;client_port={lo}-{hi}"),
            )],
        )
    }

    fn play(&mut self) -> Vec<u8> {
        // Addressed to the BASE url, not the track url — see the module docs.
        let url = self.url.clone();
        self.state = State::AwaitingPlay;
        self.request("PLAY", &url, &[("Range", "npt=0.000-".to_string())])
    }

    /// The next request the state machine wants, built and marked outstanding.
    ///
    /// Called by [`Self::handle`] the moment a response lands, so [`Self::next_request`] is not
    /// the only way bytes come out — see [`Self::pending_request`].
    fn advance(&mut self) -> Option<Vec<u8>> {
        match self.state {
            State::AwaitingDescribe => Some(self.describe()),
            State::AwaitingSetup(t) => Some(self.setup(t)),
            State::AwaitingPlay => Some(self.play()),
            _ => None,
        }
    }

    // -- response handling ---------------------------------------------------------------

    fn handle(&mut self, resp: Response) -> Result<Option<Event>, RtspError> {
        let expected = self.pending.take().ok_or(RtspError::Unsolicited)?;
        if resp.cseq != Some(expected) {
            return Err(RtspError::CseqMismatch {
                expected,
                got: resp.cseq,
            });
        }

        let state = self.state;
        match state {
            State::AwaitingOptions => {
                self.require_ok(&resp)?;
                let methods = resp
                    .header("Public")
                    .map(|p| p.split(',').map(|m| m.trim().to_string()).collect())
                    .unwrap_or_default();
                self.state = State::AwaitingDescribe;
                Ok(Some(Event::Options { methods }))
            }
            State::AwaitingDescribe => {
                self.require_ok(&resp)?;
                let text = resp.body_str().unwrap_or_default();
                let parsed = sdp::parse(text).map_err(RtspError::BadSdp)?;
                if parsed.video().is_none() {
                    return Err(RtspError::NoVideoTrack);
                }
                self.sdp = Some(parsed.clone());
                self.state = State::AwaitingSetup(Track::Video);
                Ok(Some(Event::Described(Box::new(parsed))))
            }
            State::AwaitingSetup(track) => {
                if !resp.is_ok() {
                    // A refused AUDIO setup is survivable and is the §14 rule; a refused VIDEO
                    // setup leaves nothing to play.
                    if track == Track::Audio {
                        self.state = State::AwaitingPlay;
                        return Ok(Some(Event::SetUpRefused {
                            track,
                            status: resp.status,
                        }));
                    }
                    return Err(self.status_error(&resp));
                }
                let session_id = resp
                    .header("Session")
                    .map(session_id_of)
                    .ok_or(RtspError::NoSessionHeader)?;
                self.session_id = Some(session_id.clone());
                let ports = resp
                    .header("Transport")
                    .and_then(|t| parse_transport(t).server_ports);
                match track {
                    Track::Video => {
                        self.video_server_ports = ports;
                        self.state = if self.want_audio
                            && self.sdp.as_ref().and_then(|s| s.audio()).is_some()
                        {
                            State::AwaitingSetup(Track::Audio)
                        } else {
                            State::AwaitingPlay
                        };
                    }
                    Track::Audio => {
                        self.audio_server_ports = ports;
                        self.audio_set_up = true;
                        self.state = State::AwaitingPlay;
                    }
                }
                Ok(Some(Event::SetUp {
                    track,
                    server_ports: ports,
                    session_id,
                }))
            }
            State::AwaitingPlay => {
                self.require_ok(&resp)?;
                self.state = State::Playing;
                Ok(Some(Event::Playing))
            }
            State::AwaitingTeardown => {
                self.state = State::Closed;
                Ok(Some(Event::TornDown))
            }
            State::Init | State::Playing | State::Closed => Ok(Some(Event::Other(Box::new(resp)))),
        }
    }

    fn require_ok(&self, resp: &Response) -> Result<(), RtspError> {
        if resp.is_ok() {
            Ok(())
        } else {
            Err(self.status_error(resp))
        }
    }

    fn status_error(&self, resp: &Response) -> RtspError {
        RtspError::Status {
            status: resp.status,
            reason: resp.reason.clone(),
            cseq: resp.cseq,
        }
    }

    /// Whether the audio track was successfully set up.
    pub fn audio_set_up(&self) -> bool {
        self.audio_set_up
    }

    /// The next request, built after a response has been handled.
    ///
    /// [`Self::feed`] does not send anything itself, so the loop is: feed → `pending_request` →
    /// write → read → feed. `None` means wait, play, or stop.
    pub fn pending_request(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_some() {
            return None;
        }
        match self.state {
            State::Init => self.next_request(),
            _ => self.advance(),
        }
    }
}

impl Default for RtspSession {
    fn default() -> Self {
        RtspSession::new()
    }
}

/// `82838485…;timeout=60` → `82838485…`. This server sends no parameters, but a client that
/// echoes the whole header back on PLAY breaks against servers that do.
fn session_id_of(header: &str) -> String {
    header
        .split(';')
        .next()
        .unwrap_or(header)
        .trim()
        .to_string()
}

/// Take one complete response off the front of `buffer`, if there is one.
///
/// Returns `Ok(None)` when the message is not all there yet — that is a WAIT, not a failure, and
/// keeping it distinct from an error is what lets a caller hand over whatever `read()` gave it.
pub fn take_response(buffer: &mut Vec<u8>) -> Result<Option<Response>, RtspError> {
    let Some(head_end) = find_header_end(buffer) else {
        return Ok(None);
    };
    let head = core::str::from_utf8(&buffer[..head_end]).map_err(|_| RtspError::BadStatusLine {
        line: "<not UTF-8>".to_string(),
    })?;
    let mut lines = head.split("\r\n").flat_map(|l| l.split('\n'));

    let status_line = lines.next().unwrap_or_default().trim_end();
    let (status, reason) = parse_status_line(status_line)?;

    let mut headers = Vec::new();
    let mut cseq = None;
    let mut content_length = 0usize;
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| RtspError::BadHeader {
            line: line.to_string(),
        })?;
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("CSeq") {
            cseq = v.parse().ok();
        }
        if k.eq_ignore_ascii_case("Content-Length") {
            content_length = v.parse().map_err(|_| RtspError::BadContentLength {
                value: v.to_string(),
            })?;
        }
        headers.push((k.to_string(), v.to_string()));
    }

    let body_start = head_end + header_terminator_len(buffer, head_end);
    if buffer.len() < body_start + content_length {
        return Ok(None);
    }
    let body = buffer[body_start..body_start + content_length].to_vec();
    buffer.drain(..body_start + content_length);

    Ok(Some(Response {
        status,
        reason,
        cseq,
        headers,
        body,
    }))
}

fn parse_status_line(line: &str) -> Result<(u16, String), RtspError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("RTSP/") {
        return Err(RtspError::BadStatusLine {
            line: line.to_string(),
        });
    }
    let status: u16 =
        parts
            .next()
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| RtspError::BadStatusLine {
                line: line.to_string(),
            })?;
    Ok((status, parts.next().unwrap_or("").trim().to_string()))
}

/// Offset of the blank line ending the headers. Tolerates bare-LF line endings, which no server
/// here sends and every hand-written test fixture eventually does.
fn find_header_end(b: &[u8]) -> Option<usize> {
    let crlf = b.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = b.windows(2).position(|w| w == b"\n\n");
    match (crlf, lf) {
        (Some(a), Some(c)) => Some(a.min(c)),
        (Some(a), None) => Some(a),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

fn header_terminator_len(b: &[u8], at: usize) -> usize {
    if b[at..].starts_with(b"\r\n\r\n") {
        4
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every byte below was cut from a live session with the glasses: the four requests a
    // reference client sent and the four responses the firmware returned, verbatim, including
    // the session id and the server's own ports.

    const LIVE_OPTIONS_REPLY: &str =
        "RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: DESCRIBE, SETUP, TEARDOWN, PLAY, PAUSE\r\n\r\n";

    const LIVE_SDP_BODY: &str = concat!(
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

    const LIVE_SESSION_ID: &str = "82838485868788898A8B8C8D8E8F90";

    fn live_describe_reply() -> String {
        format!(
            concat!(
                "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n",
                "Cache-Control: no-cache\r\n",
                "Server: Hisilicon RTSP Streaming Media Server/1.0.0\r\n",
                "Content-Length: {}\r\n\r\n{}"
            ),
            LIVE_SDP_BODY.len(),
            LIVE_SDP_BODY
        )
    }

    const LIVE_SETUP_REPLY: &str = concat!(
        "RTSP/1.0 200 OK\r\nCSeq: 3\r\n",
        "Transport: RTP/AVP;unicast;client_port=8712-8713;server_port=53796-53797\r\n",
        "Session: 82838485868788898A8B8C8D8E8F90\r\n\r\n",
    );

    const LIVE_PLAY_REPLY: &str =
        concat!("RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 82838485868788898A8B8C8D8E8F90\r\n\r\n",);

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("requests are ASCII")
    }

    #[test]
    fn the_whole_live_exchange_replays_request_for_request() {
        let mut s = RtspSession::new();

        let options = s.next_request().expect("OPTIONS first");
        assert_eq!(
            text(&options),
            "OPTIONS rtsp://192.168.169.1:554/h264 RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: luma-core\r\n\r\n"
        );
        assert_eq!(s.pending_cseq(), Some(1));
        assert!(
            s.next_request().is_none(),
            "one request in flight at a time"
        );

        let events = s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        assert_eq!(
            events,
            vec![Event::Options {
                methods: vec![
                    "DESCRIBE".into(),
                    "SETUP".into(),
                    "TEARDOWN".into(),
                    "PLAY".into(),
                    "PAUSE".into()
                ]
            }]
        );

        let describe = s.pending_request().expect("DESCRIBE next");
        assert_eq!(
            text(&describe),
            concat!(
                "DESCRIBE rtsp://192.168.169.1:554/h264 RTSP/1.0\r\n",
                "CSeq: 2\r\nAccept: application/sdp\r\nUser-Agent: luma-core\r\n\r\n"
            )
        );

        let events = s.feed(live_describe_reply().as_bytes()).unwrap();
        assert!(matches!(events.as_slice(), [Event::Described(_)]));
        let sdp = s.sdp().expect("the description");
        assert_eq!(sdp.video().unwrap().payload_type, 96);
        assert_eq!(sdp.audio().unwrap().payload_type, 97);

        let setup = s.pending_request().expect("SETUP track0");
        assert_eq!(
            text(&setup),
            concat!(
                "SETUP rtsp://192.168.169.1:554/h264/track0 RTSP/1.0\r\n",
                "CSeq: 3\r\nTransport: RTP/AVP/UDP;unicast;client_port=8712-8713\r\n",
                "User-Agent: luma-core\r\n\r\n"
            )
        );

        let events = s.feed(LIVE_SETUP_REPLY.as_bytes()).unwrap();
        assert_eq!(
            events,
            vec![Event::SetUp {
                track: Track::Video,
                server_ports: Some((53_796, 53_797)),
                session_id: LIVE_SESSION_ID.to_string(),
            }]
        );
        assert_eq!(s.session_id(), Some(LIVE_SESSION_ID));
        assert_eq!(s.video_server_ports(), Some((53_796, 53_797)));

        let play = s.pending_request().expect("PLAY");
        assert_eq!(
            text(&play),
            format!(
                concat!(
                    "PLAY rtsp://192.168.169.1:554/h264 RTSP/1.0\r\n",
                    "CSeq: 4\r\nRange: npt=0.000-\r\nUser-Agent: luma-core\r\nSession: {}\r\n\r\n"
                ),
                LIVE_SESSION_ID
            )
        );

        assert_eq!(
            s.feed(LIVE_PLAY_REPLY.as_bytes()).unwrap(),
            vec![Event::Playing]
        );
        assert!(s.is_playing());
        assert_eq!(s.state(), State::Playing);
        assert!(s.pending_request().is_none(), "nothing left to send");
    }

    #[test]
    fn the_session_id_goes_on_play_but_not_on_options_or_describe() {
        let mut s = RtspSession::new();
        let opts = text(&s.next_request().unwrap());
        assert!(!opts.contains("Session:"));
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        let desc = text(&s.pending_request().unwrap());
        assert!(!desc.contains("Session:"));
        s.feed(live_describe_reply().as_bytes()).unwrap();
        let setup = text(&s.pending_request().unwrap());
        assert!(!setup.contains("Session:"), "no session exists yet");
        s.feed(LIVE_SETUP_REPLY.as_bytes()).unwrap();
        assert!(
            text(&s.pending_request().unwrap()).contains(&format!("Session: {LIVE_SESSION_ID}"))
        );
    }

    #[test]
    fn a_response_split_across_reads_is_assembled_and_two_in_one_read_are_both_taken() {
        let mut s = RtspSession::new();
        s.next_request();
        let reply = LIVE_OPTIONS_REPLY.as_bytes();
        assert!(s.feed(&reply[..10]).unwrap().is_empty(), "wait, not fail");
        assert!(s.feed(&reply[10..25]).unwrap().is_empty());
        assert_eq!(s.feed(&reply[25..]).unwrap().len(), 1);

        // A body split across the header boundary.
        let describe = live_describe_reply();
        let bytes = describe.as_bytes();
        let cut = bytes.len() - 100;
        s.pending_request();
        assert!(
            s.feed(&bytes[..cut]).unwrap().is_empty(),
            "the body is short"
        );
        assert_eq!(s.feed(&bytes[cut..]).unwrap().len(), 1);
    }

    #[test]
    fn a_cseq_that_does_not_match_the_request_in_flight_is_fatal() {
        let mut s = RtspSession::new();
        s.next_request();
        let wrong = "RTSP/1.0 200 OK\r\nCSeq: 9\r\n\r\n";
        assert_eq!(
            s.feed(wrong.as_bytes()),
            Err(RtspError::CseqMismatch {
                expected: 1,
                got: Some(9)
            })
        );
    }

    #[test]
    fn a_refused_setup_stops_the_video_and_only_costs_the_audio_track() {
        // Video refused: fatal.
        let mut s = RtspSession::new();
        s.next_request();
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        s.pending_request();
        s.feed(live_describe_reply().as_bytes()).unwrap();
        s.pending_request();
        assert_eq!(
            s.feed(b"RTSP/1.0 461 Unsupported Transport\r\nCSeq: 3\r\n\r\n"),
            Err(RtspError::Status {
                status: 461,
                reason: "Unsupported Transport".into(),
                cseq: Some(3)
            })
        );

        // Audio refused: survivable, and PLAY still goes out.
        let mut s = RtspSession::new().request_audio(true);
        s.next_request();
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        s.pending_request();
        s.feed(live_describe_reply().as_bytes()).unwrap();
        s.pending_request();
        s.feed(LIVE_SETUP_REPLY.as_bytes()).unwrap();
        let audio_setup = text(&s.pending_request().expect("SETUP track1"));
        assert!(audio_setup.starts_with("SETUP rtsp://192.168.169.1:554/h264/track1"));
        assert!(audio_setup.contains("client_port=8714-8715"));
        let events = s
            .feed(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 4\r\n\r\n")
            .unwrap();
        assert_eq!(
            events,
            vec![Event::SetUpRefused {
                track: Track::Audio,
                status: 454
            }]
        );
        assert!(!s.audio_set_up());
        assert!(text(&s.pending_request().unwrap()).starts_with("PLAY"));
    }

    #[test]
    fn client_ports_are_configurable_and_default_to_the_documented_pair() {
        let s = RtspSession::new();
        assert_eq!(s.client_ports(Track::Video), (8712, 8713));
        assert_eq!(s.client_ports(Track::Audio), (8714, 8715));
        let s = RtspSession::new().video_client_ports((14_502, 14_503));
        assert_eq!(s.client_ports(Track::Video), (14_502, 14_503));
    }

    /// The `Transport` header from the live SETUP reply, and the shapes other servers send.
    #[test]
    fn the_transport_header_yields_the_server_ports() {
        let t = parse_transport("RTP/AVP;unicast;client_port=8712-8713;server_port=53796-53797");
        assert_eq!(t.protocol, "RTP/AVP");
        assert!(t.unicast);
        assert_eq!(t.client_ports, Some((8712, 8713)));
        assert_eq!(t.server_ports, Some((53_796, 53_797)));
        assert_eq!(t.ssrc, None, "this server declares none");

        let single = parse_transport("RTP/AVP/UDP;unicast;server_port=6000;ssrc=DEADBEEF");
        assert_eq!(single.server_ports, Some((6000, 6001)), "RTCP is implicit");
        assert_eq!(single.ssrc, Some(0xDEAD_BEEF));
    }

    #[test]
    fn the_session_header_is_split_from_any_timeout_parameter() {
        assert_eq!(session_id_of("ABC123"), "ABC123");
        assert_eq!(session_id_of("ABC123;timeout=60"), "ABC123");
        assert_eq!(session_id_of(" ABC123 ; timeout=60"), "ABC123");
    }

    #[test]
    fn teardown_carries_the_session_and_closes_the_machine() {
        let mut s = RtspSession::new();
        s.next_request();
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        s.pending_request();
        s.feed(live_describe_reply().as_bytes()).unwrap();
        s.pending_request();
        s.feed(LIVE_SETUP_REPLY.as_bytes()).unwrap();
        s.pending_request();
        s.feed(LIVE_PLAY_REPLY.as_bytes()).unwrap();

        let bye = text(&s.teardown());
        assert!(bye.starts_with("TEARDOWN rtsp://192.168.169.1:554/h264 RTSP/1.0\r\nCSeq: 5\r\n"));
        assert!(bye.contains(&format!("Session: {LIVE_SESSION_ID}")));
        assert_eq!(s.state(), State::AwaitingTeardown);
        assert_eq!(
            s.feed(b"RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n").unwrap(),
            vec![Event::TornDown]
        );
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn a_description_with_no_video_track_is_refused_rather_than_played() {
        let body = "v=0\r\nm=audio 0 RTP/AVP 97\r\na=rtpmap:97 mpeg4-generic/16000/1\r\n";
        let reply = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut s = RtspSession::new();
        s.next_request();
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        s.pending_request();
        assert_eq!(s.feed(reply.as_bytes()), Err(RtspError::NoVideoTrack));
    }

    #[test]
    fn a_setup_reply_with_no_session_header_is_an_error_not_a_silent_empty_string() {
        let mut s = RtspSession::new();
        s.next_request();
        s.feed(LIVE_OPTIONS_REPLY.as_bytes()).unwrap();
        s.pending_request();
        s.feed(live_describe_reply().as_bytes()).unwrap();
        s.pending_request();
        assert_eq!(
            s.feed(b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nTransport: RTP/AVP\r\n\r\n"),
            Err(RtspError::NoSessionHeader)
        );
    }

    #[test]
    fn a_dollar_sign_is_interleaved_transport_and_is_reported_rather_than_eaten() {
        let mut s = RtspSession::new();
        s.next_request();
        assert_eq!(
            s.feed(&[b'$', 0, 0, 4, 1, 2, 3, 4]),
            Err(RtspError::InterleavedNotSupported)
        );
    }

    #[test]
    fn something_that_is_not_an_rtsp_response_is_rejected_at_the_status_line() {
        let mut s = RtspSession::new();
        s.next_request();
        assert!(matches!(
            s.feed(b"HTTP/1.1 200 OK\r\nCSeq: 1\r\n\r\n"),
            Err(RtspError::BadStatusLine { .. })
        ));
    }

    #[test]
    fn the_url_helpers_agree_with_the_protocol_reference() {
        assert_eq!(URL, "rtsp://192.168.169.1:554/h264");
        assert_eq!(stream_url(crate::fileapi::HOST), URL);
        assert_eq!(PORT, 554);
    }
}
