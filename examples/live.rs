//! Record the glasses' live view to `live.h264` and `live.aac`. No dependencies.
//!
//! ```text
//! cargo run --example live                  # 10 seconds, into ./
//! cargo run --example live -- 30            # 30 seconds
//! cargo run --example live -- 30 out/       # into a directory
//! ```
//!
//! Before running this: raise the SoftAP with `0x67` over BLE (`cargo run --example luma
//! --features ble -- wifi live`, or the Python demo), join the network, then start this.
//!
//! What it does is the whole §14 path with nothing hidden: an RTSP conversation over TCP driven
//! by [`luma_core::rtsp::RtspSession`], two UDP sockets bound to the client ports, RTP parsed by
//! [`luma_core::rtp`], H.264 depacketised by [`luma_core::h264`] and AAC by
//! [`luma_core::aac`]. Every protocol decision is in the library; this file owns three sockets
//! and a loop.
//!
//! The video file starts at the first IDR that arrived with its SPS and PPS. Starting anywhere
//! else produces a grey mess until the next keyframe, which looks like a decoder bug and is not
//! one.
//!
//! Nothing here was run against hardware from the environment this was written in — there is no
//! access point and no unit. The RTSP exchange it drives is byte-for-byte the one pinned in
//! `rtsp`'s tests from a live session.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use luma_core::aac::{self, AuHeaderConfig, AudioSpecificConfig};
use luma_core::h264::Depacketizer;
use luma_core::rtp::{self, SequenceTracker};
use luma_core::rtsp::{self, Event, RtspSession, Track};
use luma_core::{fileapi, timing};

/// How long to wait for the RTSP server to answer one request.
const RTSP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the UDP read blocks before the loop checks its own clock. Small enough that the
/// recording stops on time, large enough not to spin.
const UDP_POLL: Duration = Duration::from_millis(200);

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("\n{e}");
            eprintln!(
                "\nThe glasses must be on their own Wi-Fi and this machine joined to it:\n  \
                 1. write 0x67 over BLE   (cargo run --example luma --features ble -- wifi live)\n  \
                 2. join the SSID it prints, passphrase {}\n  \
                 3. wait ~{:?} after the SSID before joining — the AP is not ready sooner\n  \
                 4. run this again",
                fileapi::PASSPHRASE,
                timing::SSID_SETTLE
            );
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map(|s| s.parse().unwrap_or(10)).unwrap_or(10);
    let dir: PathBuf = args.next().map(PathBuf::from).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    // ---- the RTSP conversation --------------------------------------------------------
    let mut session = RtspSession::new()
        .request_audio(true)
        .user_agent("luma-core/live");

    let addr = format!("{}:{}", fileapi::HOST, rtsp::PORT);
    println!("connecting to rtsp://{addr}{}", rtsp::PATH);
    let mut tcp = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("{addr}: {e}"))?,
        RTSP_TIMEOUT,
    )
    .map_err(|e| format!("connect {addr}: {e}"))?;
    tcp.set_read_timeout(Some(RTSP_TIMEOUT)).ok();

    let (video_lo, video_hi) = session.client_ports(Track::Video);
    let (audio_lo, audio_hi) = session.client_ports(Track::Audio);
    let (video_sock, _video_rtcp) = bind_pair(video_lo, video_hi)?;
    let (audio_sock, _audio_rtcp) = match bind_pair(audio_lo, audio_hi) {
        Ok((a, b)) => (Some(a), b),
        Err(_) => (None, None),
    };

    let mut audio_cfg = AuHeaderConfig::GLASSES;
    let mut audio_asc = AudioSpecificConfig::GLASSES;
    let mut audio_payload_type = 97u8;
    let mut video_payload_type = 96u8;

    let mut buf = [0u8; 4096];
    while !session.is_playing() {
        if let Some(req) = session.pending_request() {
            print!(
                "→ {}",
                String::from_utf8_lossy(&req).lines().next().unwrap_or("")
            );
            println!();
            tcp.write_all(&req).map_err(|e| format!("write: {e}"))?;
        }
        let n = tcp.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the server closed the connection".into());
        }
        for event in session.feed(&buf[..n]).map_err(|e| e.to_string())? {
            match event {
                Event::Options { methods } => println!("← OPTIONS: {}", methods.join(", ")),
                Event::Described(sdp) => {
                    if let Some(v) = sdp.video() {
                        video_payload_type = v.payload_type;
                        println!(
                            "← video pt {} {}",
                            v.payload_type,
                            v.rtpmap
                                .as_ref()
                                .map(|m| m.encoding.as_str())
                                .unwrap_or("?")
                        );
                    }
                    if let Some(a) = sdp.audio() {
                        audio_payload_type = a.payload_type;
                        if let Some(f) = &a.fmtp {
                            if let Some(c) = AuHeaderConfig::from_fmtp(f) {
                                audio_cfg = c;
                            }
                            if let Some(asc) =
                                f.config().and_then(|c| aac::parse_config_hex(c).ok())
                            {
                                audio_asc = asc;
                            }
                        }
                        println!(
                            "← audio pt {} {} Hz, {} ch",
                            a.payload_type,
                            audio_asc.sample_rate_hz,
                            audio_asc.channel_configuration
                        );
                    }
                }
                Event::SetUp {
                    track,
                    server_ports,
                    ..
                } => println!("← SETUP {track:?}, server ports {server_ports:?}"),
                Event::SetUpRefused { track, status } => {
                    println!("← SETUP {track:?} refused ({status}) — carrying on without it")
                }
                Event::Playing => println!("← PLAY, streaming"),
                other => println!("← {other:?}"),
            }
        }
    }

    // ---- the media loop ---------------------------------------------------------------
    video_sock.set_read_timeout(Some(UDP_POLL)).ok();
    if let Some(s) = &audio_sock {
        s.set_read_timeout(Some(UDP_POLL)).ok();
    }

    let video_path = dir.join("live.h264");
    let audio_path = dir.join("live.aac");
    let mut video_file = File::create(&video_path).map_err(|e| e.to_string())?;
    let mut audio_file: Option<File> = None;

    let mut depack = Depacketizer::new();
    let mut aac_depack = aac::Depacketizer::with_config(audio_cfg);
    let mut video_seq = SequenceTracker::new();
    let mut audio_seq = SequenceTracker::new();

    let mut frames = 0u64;
    let mut video_bytes = 0u64;
    let mut audio_frames = 0u64;
    let mut dropped_before_keyframe = 0u64;
    let mut first_frame_at: Option<Instant> = None;

    let started = Instant::now();
    let stop_after = Duration::from_secs(seconds);
    println!("\nrecording {seconds}s… (the file starts at the first keyframe)");

    let mut pkt = vec![0u8; 65_536];
    while started.elapsed() < stop_after {
        // Video.
        if let Ok(n) = video_sock.recv(&mut pkt) {
            if let Ok(p) = rtp::parse(&pkt[..n]) {
                if p.header.payload_type == video_payload_type {
                    video_seq.observe(p.header.sequence);
                }
            }
            for au in depack.push(&pkt[..n]) {
                if !au.is_decodable_start() && frames == 0 {
                    // Everything before the first IDR-with-parameter-sets is unplayable.
                    dropped_before_keyframe += 1;
                    continue;
                }
                if frames == 0 {
                    first_frame_at = Some(Instant::now());
                    println!("  first keyframe: {} bytes", au.len());
                }
                video_file.write_all(&au.data).map_err(|e| e.to_string())?;
                video_bytes += au.data.len() as u64;
                frames += 1;
            }
        }

        // Audio, best-effort. A refused SETUP or a silent track costs nothing here.
        if let Some(s) = &audio_sock {
            if let Ok(n) = s.recv(&mut pkt) {
                if let Ok(p) = rtp::parse(&pkt[..n]) {
                    if p.header.payload_type == audio_payload_type {
                        audio_seq.observe(p.header.sequence);
                    }
                }
                for frame in aac_depack.push(&pkt[..n]) {
                    let f = audio_file.get_or_insert_with(|| {
                        File::create(&audio_path).expect("create the audio file")
                    });
                    match aac::adts_frame(&audio_asc, &frame) {
                        Ok(adts) => {
                            f.write_all(&adts).map_err(|e| e.to_string())?;
                            audio_frames += 1;
                        }
                        Err(e) => eprintln!("  audio frame skipped: {e}"),
                    }
                }
            }
        }
    }

    // ---- close down -------------------------------------------------------------------
    let bye = session.teardown();
    let _ = tcp.write_all(&bye);
    let _ = tcp.read(&mut buf);

    if let Some(au) = depack.flush() {
        if depack.keyframe_seen() {
            let _ = video_file.write_all(&au.data);
        }
    }
    video_file.flush().ok();

    let elapsed = first_frame_at
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0)
        .max(0.001);
    let stats = depack.stats();

    println!(
        "\n{} — {frames} pictures, {video_bytes} bytes",
        video_path.display()
    );
    println!(
        "  {:.1} fps, {:.2} Mbit/s over {:.1}s",
        frames as f64 / elapsed,
        (video_bytes as f64 * 8.0) / elapsed / 1_000_000.0,
        elapsed
    );
    println!(
        "  RTP: {} packets, {} lost ({:.2}%), {} reordered, {} duplicated",
        video_seq.received(),
        video_seq.lost_estimate(),
        video_seq.loss_fraction() * 100.0,
        video_seq.reordered(),
        video_seq.duplicates()
    );
    println!(
        "  depacketiser: {} keyframes, {} NALs, {} fragments dropped, {} packets unsupported",
        stats.keyframes, stats.nal_units, stats.dropped_fragments, stats.unsupported_packets
    );
    if dropped_before_keyframe > 0 {
        println!("  {dropped_before_keyframe} pictures discarded before the first keyframe");
    }
    if audio_frames > 0 {
        println!(
            "\n{} — {audio_frames} AAC frames ({:.1}s at {} Hz)",
            audio_path.display(),
            audio_frames as f64 * aac::SAMPLES_PER_FRAME as f64 / audio_asc.sample_rate_hz as f64,
            audio_asc.sample_rate_hz
        );
        println!(
            "  RTP: {} packets, {} lost",
            audio_seq.received(),
            audio_seq.lost_estimate()
        );
    } else {
        println!("\nno audio: the track was refused, or the glasses sent none.");
    }

    if frames == 0 {
        println!("\nNo video arrived. The RTSP conversation succeeded, so the AP is up and the");
        println!("server is talking — check that nothing is dropping UDP to ports {video_lo}-{video_hi}.");
    } else {
        println!("\n  ffplay {}", video_path.display());
        if audio_frames > 0 {
            println!("  ffplay {}", audio_path.display());
        }
    }
    println!("\nRemember to write 0x44 30 00 over BLE and leave the network.");
    Ok(())
}

/// Bind the RTP port and, if it is free, the RTCP port beside it.
///
/// RTCP is bound and never read: leaving it closed makes the host answer the server's RTCP with
/// ICMP port-unreachable, which some servers treat as a disconnect.
fn bind_pair(rtp_port: u16, rtcp_port: u16) -> Result<(UdpSocket, Option<UdpSocket>), String> {
    let rtp =
        UdpSocket::bind(("0.0.0.0", rtp_port)).map_err(|e| format!("bind UDP {rtp_port}: {e}"))?;
    // Best effort; a busy RTCP port is not fatal. The socket is kept alive by the caller.
    let rtcp = UdpSocket::bind(("0.0.0.0", rtcp_port)).ok();
    Ok((rtp, rtcp))
}
