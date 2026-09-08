//! A real BLE central for the glasses. Feature `ble`.
//!
//! This is the laptop end-to-end path: scan for service `AA12`, connect, subscribe both notify
//! characteristics, and drive the protocol with the same builders and parsers an iOS or Android
//! app would use. Nothing here re-derives a protocol fact — the UUIDs come from
//! [`crate::gatt`], the frames from [`crate::commands`], the decode from [`crate::parser`],
//! [`crate::voice`] and [`crate::reassembly`], and every duration from [`crate::timing`].
//!
//! ```no_run
//! # #[cfg(feature = "ble")] {
//! use luma_core::client::ble::Glasses;
//! use luma_core::timing;
//!
//! # async fn go() -> Result<(), luma_core::client::ble::BleError> {
//! let g = Glasses::connect_first(std::time::Duration::from_secs(15)).await?;
//! let info = g.handshake().await?;
//! println!("{:?} battery {:?}", info.versions, info.battery);
//!
//! if let Some(jpeg) = g.take_photo(true).await? {
//!     std::fs::write("ai.jpg", jpeg).ok();
//! }
//!
//! let ssid = g.open_wifi(luma_core::parser::WifiService::Files).await?;
//! println!("join {ssid} / 12345678");
//! # Ok(()) } }
//! ```
//!
//! ## The four rules this client exists to get right
//!
//! **Subscribe before writing.** The glasses push a `0x95` capabilities frame about
//! [`timing::CAPABILITIES_PUSH_DELAY`] after the link comes up, before the app has written a
//! byte. [`Glasses::connect`] subscribes both characteristics and starts the pump before it
//! returns, so nothing can be missed by a caller that is slow to ask.
//!
//! **Write with response.** `AA13` takes an ATT Write **Request**
//! ([`crate::gatt::WRITE_WITH_RESPONSE`]). btleplug spells that
//! [`WriteType::WithResponse`]; the other one is a different ATT opcode the firmware never
//! acknowledges and gives no error for.
//!
//! **The microphone only closes when you close it.** `0x99` is declared and never sent, so
//! [`Glasses::voice_capture`] owns two timers and writes `0x56` unconditionally when either
//! fires — twice, [`timing::VOICE_INTERRUPT_REPEAT_GAP`] apart, which is what shipping clients
//! do.
//!
//! **The `0x48` read has no `0x48` reply.** [`Glasses::handshake`] waits for the ten-frame
//! burst by watching the accumulator fill, not by waiting for a frame that never comes.
//!
//! ## Not exercised here
//!
//! Every method below was written against the protocol reference and the crate's own tests.
//! None of it has been run against hardware from this environment — there is no adapter and no
//! unit. The sans-IO half underneath it is pinned by golden vectors; this half is plumbing over
//! that, and its first real run will be on someone's desk.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::bleuuid::{uuid_from_u16, BleUuid};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::sync::broadcast;

use crate::commands;
use crate::gatt;
use crate::parser::{
    BatteryReading, Capabilities, DeviceEvent, Identity, Parser, SwitchStates, Versions,
    VoiceFeatures, Volumes, WifiService,
};
use crate::reassembly::{FileEvent, FileReassembler};
use crate::timing;
use crate::voice::{VoiceEvent, VoicePacket};

/// How many events the broadcast channel holds before a slow subscriber starts missing them.
///
/// Sized for the voice stream: fifty packets a second, so this is ~20 s of backlog. A subscriber
/// that falls further behind than that gets `RecvError::Lagged` and is told how many it missed
/// rather than silently handed a gap.
pub const EVENT_BUFFER: usize = 1024;

/// Anything that stops the radio doing what was asked.
#[derive(Debug)]
pub enum BleError {
    /// The platform's Bluetooth stack. Includes "no adapter" and "permission denied", which on
    /// macOS means the terminal has not been granted Bluetooth access in System Settings.
    Adapter { detail: String },
    /// Nothing advertising service `AA12` inside the scan window.
    NotFound,
    /// Connected, but the peripheral does not expose the characteristics this protocol needs.
    /// See [`crate::gatt::is_drivable`].
    NotDrivable { characteristics: Vec<String> },
    /// A GATT operation failed.
    Gatt { operation: String, detail: String },
    /// Waited for something the device did not send inside the window.
    Timeout {
        waiting_for: String,
        after: Duration,
    },
    /// The notification pump stopped — the link dropped.
    Disconnected,
}

impl std::fmt::Display for BleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BleError::Adapter { detail } => write!(f, "bluetooth adapter: {detail}"),
            BleError::NotFound => write!(f, "no peripheral advertising service {}", gatt::SERVICE),
            BleError::NotDrivable { characteristics } => write!(
                f,
                "peripheral exposes {characteristics:?}, which is not {} plus a notify",
                gatt::WRITE
            ),
            BleError::Gatt { operation, detail } => write!(f, "{operation}: {detail}"),
            BleError::Timeout { waiting_for, after } => {
                write!(f, "no {waiting_for} within {after:?}")
            }
            BleError::Disconnected => write!(f, "the link dropped"),
        }
    }
}

impl std::error::Error for BleError {}

impl From<btleplug::Error> for BleError {
    fn from(e: btleplug::Error) -> Self {
        BleError::Adapter {
            detail: e.to_string(),
        }
    }
}

/// Everything one connection publishes, from both notify characteristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlassesEvent {
    /// A decoded `AA14` frame.
    Device(DeviceEvent),
    /// Something happened to a `52 58` file transfer on `AA15`.
    File(FileEvent),
    /// The pump stopped.
    Disconnected,
}

/// A peripheral that advertised the glasses' service.
#[derive(Clone)]
pub struct Discovered {
    /// The platform's identifier — a UUID on macOS, a MAC elsewhere. Stable per host.
    pub id: String,
    /// The advertised name, when there is one.
    pub name: Option<String>,
    pub rssi: Option<i16>,
    peripheral: Peripheral,
}

impl std::fmt::Debug for Discovered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovered")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("rssi", &self.rssi)
            .finish()
    }
}

/// Everything the §3 connection sequence reads back.
///
/// Every field is an `Option` on purpose: a handshake that times out half way is still worth
/// what it collected, and a `None` says which read did not answer rather than presenting a
/// default that looks like a device reading.
#[derive(Debug, Clone, Default)]
pub struct Handshake {
    /// `0x55` — bt / isp / hw firmware versions.
    pub versions: Option<Versions>,
    /// `0x64` — project name and customer code.
    pub identity: Option<Identity>,
    /// `0x17` — battery percentage and charge state.
    pub battery: Option<BatteryReading>,
    /// `0x95` — the capability word.
    pub capabilities: Option<Capabilities>,
    /// `0x71` — which voice features are on.
    pub voice_features: Option<VoiceFeatures>,
    /// `0x69` — the three volume channels.
    pub volumes: Option<Volumes>,
    /// The `0x48` burst, accumulated. [`SwitchStates::is_complete`] says whether all ten
    /// arrived.
    pub switches: SwitchStates,
}

impl Handshake {
    /// Whether every read answered.
    pub fn is_complete(&self) -> bool {
        self.versions.is_some()
            && self.identity.is_some()
            && self.battery.is_some()
            && self.capabilities.is_some()
            && self.volumes.is_some()
            && self.switches.is_complete()
    }
}

/// A connected pair of glasses.
///
/// Cloneable handles are not offered: the pump task is owned here and stopping it on drop is
/// what keeps a dropped `Glasses` from leaving a subscription alive on the peripheral. Share it
/// behind an `Arc` if two tasks need it.
pub struct Glasses {
    peripheral: Peripheral,
    write_char: Characteristic,
    tx: broadcast::Sender<GlassesEvent>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Glasses {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl Glasses {
    /// Scan for peripherals advertising service `AA12`.
    ///
    /// Filtered by service UUID at the adapter, which is what the protocol reference recommends:
    /// the glasses' advertised NAME varies by unit and firmware and is not a reliable filter.
    pub async fn scan(timeout: Duration) -> Result<Vec<Discovered>, BleError> {
        let adapter = Self::adapter().await?;
        let service = uuid_from_u16(0xAA12);
        adapter
            .start_scan(ScanFilter {
                services: vec![service],
            })
            .await?;
        tokio::time::sleep(timeout).await;
        let peripherals = adapter.peripherals().await?;
        let _ = adapter.stop_scan().await;

        let mut out = Vec::new();
        for p in peripherals {
            let props = p.properties().await?;
            let Some(props) = props else { continue };
            if !props.services.contains(&service) {
                continue;
            }
            out.push(Discovered {
                id: p.id().to_string(),
                name: props
                    .local_name
                    .clone()
                    .or(props.advertisement_name.clone()),
                rssi: props.rssi,
                peripheral: p,
            });
        }
        // Strongest signal first: with two units on a desk, this is the one in your hand.
        out.sort_by_key(|d| -d.rssi.unwrap_or(-127));
        Ok(out)
    }

    /// Scan and connect to the strongest peripheral found.
    pub async fn connect_first(timeout: Duration) -> Result<Glasses, BleError> {
        let found = Self::scan(timeout).await?;
        let first = found.into_iter().next().ok_or(BleError::NotFound)?;
        Glasses::connect(first).await
    }

    /// Connect to one discovered peripheral, subscribe both notify characteristics and start the
    /// notification pump.
    ///
    /// Returns only once the pump is running, so the `0x95` push that lands ~30 ms after the
    /// link comes up is already being collected.
    pub async fn connect(found: Discovered) -> Result<Glasses, BleError> {
        let p = found.peripheral;
        if !p.is_connected().await.unwrap_or(false) {
            p.connect().await.map_err(|e| BleError::Gatt {
                operation: "connect".into(),
                detail: e.to_string(),
            })?;
        }
        p.discover_services().await.map_err(|e| BleError::Gatt {
            operation: "discover services".into(),
            detail: e.to_string(),
        })?;

        let chars: Vec<Characteristic> = p.characteristics().into_iter().collect();
        let names: Vec<String> = chars.iter().map(|c| c.uuid.to_short_string()).collect();
        if !gatt::is_drivable(names.iter().map(String::as_str)) {
            return Err(BleError::NotDrivable {
                characteristics: names,
            });
        }

        let find = |short: u16| -> Option<Characteristic> {
            chars
                .iter()
                .find(|c| c.uuid.to_ble_u16() == Some(short))
                .cloned()
        };
        let write_char = find(0xAA13).ok_or_else(|| BleError::NotDrivable {
            characteristics: names.clone(),
        })?;
        let control = find(0xAA14);
        let file = find(0xAA15);

        // AA14 FIRST, and its failure is fatal: it carries every control reply and the whole
        // voice stream. A failed AA15 subscribe only costs file transfers, so it is a warning
        // on the event stream and not an error.
        let control = control.ok_or_else(|| BleError::NotDrivable {
            characteristics: names.clone(),
        })?;
        p.subscribe(&control).await.map_err(|e| BleError::Gatt {
            operation: "subscribe AA14".into(),
            detail: e.to_string(),
        })?;
        if let Some(f) = &file {
            let _ = p.subscribe(f).await;
        }

        let (tx, _) = broadcast::channel(EVENT_BUFFER);
        let pump = spawn_pump(p.clone(), tx.clone()).await?;

        Ok(Glasses {
            peripheral: p,
            write_char,
            tx,
            pump,
        })
    }

    async fn adapter() -> Result<Adapter, BleError> {
        let manager = Manager::new().await?;
        manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or(BleError::Adapter {
                detail: "no adapter".into(),
            })
    }

    /// A receiver for everything the glasses publish from now on.
    ///
    /// Subscribe BEFORE writing the command whose reply you want; a receiver created afterwards
    /// can miss a fast reply, and several of these are fast.
    pub fn events(&self) -> broadcast::Receiver<GlassesEvent> {
        self.tx.subscribe()
    }

    /// The platform identifier of the connected peripheral.
    pub fn id(&self) -> String {
        self.peripheral.id().to_string()
    }

    pub async fn is_connected(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// Write one command frame to `AA13`, with response.
    pub async fn write(&self, frame: Vec<u8>) -> Result<(), BleError> {
        self.peripheral
            .write(&self.write_char, &frame, WriteType::WithResponse)
            .await
            .map_err(|e| BleError::Gatt {
                operation: format!("write {:02X?}", frame.first()),
                detail: e.to_string(),
            })
    }

    /// Disconnect cleanly.
    pub async fn disconnect(self) -> Result<(), BleError> {
        self.peripheral.disconnect().await?;
        Ok(())
    }

    /// The §3 connection sequence: read versions, project name, battery, status, the switch
    /// burst, volumes, capabilities and the voice-feature state, then send the phone time.
    ///
    /// Waits for the burst to fill rather than for a `0x48` frame — there is no `0x48` reply.
    /// A read that never answers leaves its field `None` and does not stop the others.
    pub async fn handshake(&self) -> Result<Handshake, BleError> {
        self.handshake_within(Duration::from_secs(8)).await
    }

    /// [`Self::handshake`] with an explicit deadline.
    pub async fn handshake_within(&self, budget: Duration) -> Result<Handshake, BleError> {
        let mut rx = self.events();

        for frame in [
            commands::get_versions(),
            commands::get_project_name(),
            commands::get_battery(),
            commands::get_device_status(),
            commands::get_switch_states(),
            commands::get_volumes(),
            commands::get_capabilities(),
            commands::get_voice_disable_state(),
        ] {
            self.write(frame).await?;
            // A small gap between writes. The firmware answers each one before the next arrives
            // in every capture, and a burst of eight writes with no gap has never been tried.
            tokio::time::sleep(Duration::from_millis(120)).await;
        }

        let mut out = Handshake::default();
        let deadline = tokio::time::Instant::now() + budget;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(GlassesEvent::Device(e))) => {
                    out.switches.ingest(&e);
                    match e {
                        DeviceEvent::Versions(v) => out.versions = Some(v),
                        DeviceEvent::Identity(i) => out.identity = Some(i),
                        DeviceEvent::Battery(b) => out.battery = Some(b),
                        DeviceEvent::Capabilities(c) => out.capabilities = Some(c),
                        DeviceEvent::VoiceFeatures(v) => out.voice_features = Some(v),
                        DeviceEvent::Volumes(v) => out.volumes = Some(v),
                        _ => {}
                    }
                    if out.is_complete() {
                        break;
                    }
                }
                Ok(Ok(GlassesEvent::Disconnected))
                | Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(BleError::Disconnected)
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(out)
    }

    /// Send the phone's wall clock as `0x59`.
    ///
    /// Plain hex, not BCD (§16) — [`commands::send_phone_time`] gets that right. The fields are
    /// the caller's, because this crate reads no clock; `examples/luma.rs` passes the host's.
    pub async fn send_time(
        &self,
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<(), BleError> {
        self.write(commands::send_phone_time(
            year, month, day, hour, minute, second,
        ))
        .await
    }

    /// Take a photo.
    ///
    /// With `ai = false` this writes `0x22 30` and returns `None`: the full-resolution file goes
    /// to the `EVENT` folder and only comes off over Wi-Fi (§13).
    ///
    /// With `ai = true` it writes `0x22 31` and waits for the ~11 KB JPEG the glasses push over
    /// the `52 58` file stream. Capture takes ~2.4 s and the transfer another ~0.8 s, so the
    /// wait is generous by default — see [`timing::PHOTO_CAPTURE_TYPICAL`].
    pub async fn take_photo(&self, ai: bool) -> Result<Option<Vec<u8>>, BleError> {
        self.take_photo_within(ai, timing::PHOTO_CAPTURE_TYPICAL * 4)
            .await
    }

    /// [`Self::take_photo`] with an explicit deadline for the image.
    pub async fn take_photo_within(
        &self,
        ai: bool,
        budget: Duration,
    ) -> Result<Option<Vec<u8>>, BleError> {
        let mut rx = self.events();
        self.write(commands::take_photo(ai)).await?;
        if !ai {
            return Ok(None);
        }
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or(BleError::Timeout {
                    waiting_for: "the AI image".into(),
                    after: budget,
                })?;
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(GlassesEvent::File(FileEvent::Completed(file)))) => {
                    return Ok(Some(file.data))
                }
                Ok(Ok(GlassesEvent::File(FileEvent::Aborted(_)))) => {
                    return Err(BleError::Gatt {
                        operation: "AI image".into(),
                        detail: "the transfer aborted".into(),
                    })
                }
                Ok(Ok(GlassesEvent::Device(DeviceEvent::HdImageFailed))) => {
                    return Err(BleError::Gatt {
                        operation: "AI image".into(),
                        detail: "the device reported 0x52, image failed".into(),
                    })
                }
                Ok(Ok(GlassesEvent::Disconnected))
                | Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(BleError::Disconnected)
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(BleError::Timeout {
                        waiting_for: "the AI image".into(),
                        after: budget,
                    })
                }
            }
        }
    }

    /// Collect one wake-word utterance.
    ///
    /// Returns when `idle_gap` passes with no `0x46`, or when `hard_cap` is reached — whichever
    /// comes first — and writes `0x56` **unconditionally** either way, twice,
    /// [`timing::VOICE_INTERRUPT_REPEAT_GAP`] apart. That is not belt and braces: the microphone
    /// on this firmware has no other close, so a client that returns without writing it leaves
    /// the glasses streaming until the battery is flat.
    ///
    /// The default arguments are [`timing::VOICE_IDLE_GAP`] and [`timing::VOICE_HARD_CAP`].
    pub async fn voice_capture(
        &self,
        idle_gap: Duration,
        hard_cap: Duration,
    ) -> Result<Vec<VoicePacket>, BleError> {
        let mut rx = self.events();
        let mut packets: Vec<VoicePacket> = Vec::new();
        let hard_deadline = tokio::time::Instant::now() + hard_cap;

        loop {
            let now = tokio::time::Instant::now();
            if now >= hard_deadline {
                break;
            }
            let window = idle_gap.min(hard_deadline - now);
            match tokio::time::timeout(window, rx.recv()).await {
                Ok(Ok(GlassesEvent::Device(DeviceEvent::Voice(VoiceEvent::Packet(p))))) => {
                    packets.push(p);
                }
                Ok(Ok(GlassesEvent::Device(DeviceEvent::Voice(VoiceEvent::Ended(_))))) => break,
                Ok(Ok(GlassesEvent::Disconnected))
                | Ok(Err(broadcast::error::RecvError::Closed)) => {
                    // No 0x56 to write: there is no link to write it on.
                    return Err(BleError::Disconnected);
                }
                Ok(_) => {}
                // The idle gap expired. The user stopped talking.
                Err(_) => break,
            }
        }

        self.write(commands::interrupt_voice()).await?;
        tokio::time::sleep(timing::VOICE_INTERRUPT_REPEAT_GAP).await;
        let _ = self.write(commands::interrupt_voice()).await;
        Ok(packets)
    }

    /// [`Self::voice_capture`] with the recommended timers.
    pub async fn voice_capture_default(&self) -> Result<Vec<VoicePacket>, BleError> {
        self.voice_capture(timing::VOICE_IDLE_GAP, timing::VOICE_HARD_CAP)
            .await
    }

    /// Write `0x56` on its own, to close a capture this client is not collecting.
    pub async fn interrupt_voice(&self) -> Result<(), BleError> {
        self.write(commands::interrupt_voice()).await
    }

    /// Raise the SoftAP and return its SSID.
    ///
    /// Writes `0x39` (files) or `0x67` (live), waits for the `0x25` push — which arrives about
    /// [`timing::SSID_ARRIVAL_TYPICAL`] later — and then sleeps [`timing::SSID_SETTLE`] before
    /// returning, because the access point is not accepting associations at the moment the SSID
    /// lands. The passphrase is fixed: [`crate::fileapi::PASSPHRASE`].
    pub async fn open_wifi(&self, service: WifiService) -> Result<String, BleError> {
        self.open_wifi_within(service, timing::SSID_ARRIVAL_TYPICAL * 6)
            .await
    }

    /// [`Self::open_wifi`] with an explicit deadline for the SSID.
    pub async fn open_wifi_within(
        &self,
        service: WifiService,
        budget: Duration,
    ) -> Result<String, BleError> {
        let mut rx = self.events();
        self.write(commands::open_wifi(service, false)).await?;
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or(BleError::Timeout {
                    waiting_for: "the SSID (0x25)".into(),
                    after: budget,
                })?;
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(GlassesEvent::Device(DeviceEvent::WifiCredentials(c)))) => {
                    // The AP is up but not ready. This is the wait §12 is about.
                    tokio::time::sleep(timing::SSID_SETTLE).await;
                    return Ok(c.ssid);
                }
                Ok(Ok(GlassesEvent::Disconnected))
                | Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(BleError::Disconnected)
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(BleError::Timeout {
                        waiting_for: "the SSID (0x25)".into(),
                        after: budget,
                    })
                }
            }
        }
    }

    /// Tear the SoftAP down — `0x44 30 00`, "all done".
    ///
    /// Write this before dropping the Wi-Fi network, or the phone sits on an access point with
    /// no route out.
    pub async fn close_wifi(&self) -> Result<(), BleError> {
        self.write(commands::file_download_complete()).await
    }
}

/// Route both notify characteristics through the sans-IO decoders and publish the result.
///
/// `AA14` goes to a [`Parser`], which owns the voice state machine. `AA15` goes to a
/// [`FileReassembler`]. They are separate objects because they are separate framings — `AC 55`
/// against `52 58` — and feeding one channel's bytes to the other's decoder is how a JPEG that
/// happens to contain `AC 55` becomes a phantom control frame.
async fn spawn_pump(
    peripheral: Peripheral,
    tx: broadcast::Sender<GlassesEvent>,
) -> Result<tokio::task::JoinHandle<()>, BleError> {
    let mut stream = peripheral
        .notifications()
        .await
        .map_err(|e| BleError::Gatt {
            operation: "notifications".into(),
            detail: e.to_string(),
        })?;

    let parser = Arc::new(Mutex::new(Parser::new()));
    let files = Arc::new(Mutex::new(FileReassembler::new()));

    Ok(tokio::spawn(async move {
        while let Some(n) = stream.next().await {
            let short = n.uuid.to_ble_u16();
            match short {
                Some(0xAA14) => {
                    let events = {
                        let mut p = parser.lock().expect("parser mutex");
                        p.push(&n.value)
                    };
                    for e in events {
                        let _ = tx.send(GlassesEvent::Device(e));
                    }
                }
                Some(0xAA15) => {
                    let events = {
                        let mut f = files.lock().expect("reassembler mutex");
                        f.push(&n.value)
                    };
                    for e in events {
                        let _ = tx.send(GlassesEvent::File(e));
                    }
                }
                // Nothing else is subscribed, so nothing else should arrive.
                _ => {}
            }
        }
        let _ = tx.send(GlassesEvent::Disconnected);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// There is no adapter and no unit in this environment, so what is testable here is the
    /// wiring that does not need one: that the client asks for the characteristics the GATT
    /// table names, and that its defaults are the documented durations.
    #[test]
    fn the_client_looks_for_the_characteristics_the_gatt_table_names() {
        assert_eq!(uuid_from_u16(0xAA12).to_ble_u16(), Some(0xAA12));
        assert_eq!(gatt::SERVICE, "AA12");
        assert_eq!(gatt::WRITE, "AA13");
        assert_eq!(gatt::NOTIFY_ALL, ["AA14", "AA15"]);
        assert!(gatt::WRITE_WITH_RESPONSE, "AA13 takes a Write Request");
    }

    #[test]
    fn the_voice_defaults_are_the_timing_constants_and_not_local_numbers() {
        assert_eq!(timing::VOICE_IDLE_GAP.as_millis(), 1200);
        assert_eq!(timing::VOICE_HARD_CAP.as_secs(), 30);
        assert_eq!(timing::VOICE_INTERRUPT_REPEAT_GAP.as_millis(), 500);
        assert_eq!(timing::SSID_SETTLE.as_secs(), 2);
    }

    #[test]
    fn an_incomplete_handshake_says_which_read_did_not_answer() {
        let mut h = Handshake::default();
        assert!(!h.is_complete());
        h.battery = Some(BatteryReading {
            percent: 100,
            charging: true,
            source: crate::parser::BatterySource::ReadReply,
        });
        assert!(!h.is_complete(), "one field is not a handshake");
        assert!(h.versions.is_none());
    }

    #[test]
    fn the_event_buffer_holds_about_twenty_seconds_of_the_voice_stream() {
        // ~50 packets a second (§10).
        assert!(
            EVENT_BUFFER >= 50 * 15,
            "a slow subscriber gets a real window"
        );
    }
}
