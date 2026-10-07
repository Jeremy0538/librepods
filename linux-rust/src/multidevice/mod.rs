//! Multi-device switching: the AirPods follow whichever device is playing, the way Apple
//! devices hand them around. Playback starting here connects them (holding MPRIS players
//! paused until their output appears); when another device (LibrePods on the phone) takes
//! them, playback here pauses and a notification offers to move them back. `policy` makes
//! the decisions; this module observes the system once a second and carries them out.
//! Port of the OmaPods MultiDeviceController (airpods-handover). Off unless `multi_device`
//! is true in app_settings.json.

pub mod policy;

use crate::devices::enums::{DeviceData, DeviceType};
use crate::media_controller::{any_stream_playing_sync, mpris_call_sync, playing_players_sync, sink_names_sync};
use crate::utils::{get_devices_path, multi_device_enabled};
use bluer::{Adapter, Address};
use log::{info, warn};
use policy::{Action, Buds, Observation, SwitchPolicy};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The AirPods sometimes ignore the first page while another device is using them.
const CONNECT_ATTEMPTS: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The PipeWire output appears a moment after BlueZ reports the connection.
const SINK_WAIT: Duration = Duration::from_secs(8);
/// Out of the case the AirPods advertise continuously; this long without a BLE update and
/// the last state they reported no longer counts.
const BLE_STALE: Duration = Duration::from_secs(30);

struct BudsReport {
    buds: Buds,
    seen: Instant,
}

static BUDS: Mutex<Option<BudsReport>> = Mutex::new(None);

/// Battery state from the control link (connected) or BLE adverts (disconnected).
pub fn report_buds(buds: Buds) {
    if buds == Buds::Unknown {
        return;
    }
    *BUDS.lock().unwrap() = Some(BudsReport { buds, seen: Instant::now() });
}

/// Any BLE update from our AirPods (RSSI included): they are still advertising.
pub fn report_seen() {
    if let Some(report) = BUDS.lock().unwrap().as_mut() {
        report.seen = Instant::now();
    }
}

fn current_buds(connected: bool) -> Buds {
    match BUDS.lock().unwrap().as_ref() {
        Some(r) if connected || r.seen.elapsed() < BLE_STALE => r.buds,
        _ => Buds::Unknown,
    }
}

fn airpods_address() -> Option<Address> {
    let json = std::fs::read_to_string(get_devices_path()).ok()?;
    let devices: HashMap<String, DeviceData> = serde_json::from_str(&json).ok()?;
    devices
        .iter()
        .find(|(_, d)| d.type_ == DeviceType::AirPods)
        .and_then(|(mac, _)| mac.parse().ok())
}

fn sink_prefix(address: Address) -> String {
    format!("bluez_output.{}", address.to_string().replace(':', "_"))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

async fn playing_players() -> Vec<String> {
    blocking(playing_players_sync).await.unwrap_or_default()
}

async fn pause_players() -> Vec<String> {
    let mut paused = Vec::new();
    for service in playing_players().await {
        let s = service.clone();
        if blocking(move || mpris_call_sync(&s, "Pause")).await.unwrap_or(false) {
            info!("Paused {}", service);
            paused.push(service);
        }
    }
    paused
}

async fn resume_players(players: Vec<String>) {
    for service in players {
        let s = service.clone();
        if blocking(move || mpris_call_sync(&s, "Play")).await.unwrap_or(false) {
            info!("Resumed {}", service);
        }
    }
}

async fn bluetoothctl(command: &str, address: Address) -> bool {
    let run = Command::new("bluetoothctl").arg(command).arg(address.to_string()).kill_on_drop(true).output();
    matches!(tokio::time::timeout(CONNECT_TIMEOUT, run).await, Ok(Ok(out)) if out.status.success())
}

/// A notification; with `move_back`, its "Move back" button sends on `move_back_tx`.
fn notify(summary: &'static str, body: &'static str, move_back: Option<mpsc::UnboundedSender<()>>) {
    tokio::spawn(async move {
        let mut cmd = Command::new("notify-send");
        cmd.args(["-a", "AirPods", "-i", "audio-headphones"]);
        if move_back.is_some() {
            // -A makes notify-send wait and print the key of the action clicked.
            cmd.args(["-t", "10000", "-A", "back=Move back"]);
        } else {
            cmd.args(["-t", "3000"]);
        }
        cmd.arg(summary);
        if !body.is_empty() {
            cmd.arg(body);
        }
        if let Ok(out) = cmd.output().await
            && String::from_utf8_lossy(&out.stdout).trim() == "back"
            && let Some(tx) = move_back
        {
            let _ = tx.send(());
        }
    });
}

struct Controller {
    adapter: Adapter,
    policy: SwitchPolicy,
    enabled: bool,
    /// Paused when the AirPods left; Move back resumes them, which reconnects.
    handed_over_players: Vec<String>,
    move_back_tx: mpsc::UnboundedSender<()>,
}

impl Controller {
    async fn observe(&self, address: Address) -> Observation {
        let connected = match self.adapter.device(address) {
            Ok(device) => device.is_connected().await.unwrap_or(false),
            Err(_) => false,
        };
        let own = sink_prefix(address);
        let sinks = blocking(sink_names_sync).await.unwrap_or_default();
        Observation {
            now_ms: now_ms(),
            connected,
            stream_playing: blocking(any_stream_playing_sync).await.unwrap_or(false),
            player_playing: !playing_players().await.is_empty(),
            buds: current_buds(connected),
            other_audio_connected: sinks.iter().any(|s| s.starts_with("bluez_output.") && !s.starts_with(&own)),
            powered: self.adapter.is_powered().await.unwrap_or(true),
        }
    }

    async fn evaluate(&mut self) {
        let enabled = multi_device_enabled();
        if enabled != self.enabled {
            info!("Multi-device switching {}", if enabled { "enabled" } else { "disabled" });
            self.enabled = enabled;
            // Fresh state, so whatever is playing right now doesn't count as just started.
            self.policy = SwitchPolicy::default();
        }
        if !enabled {
            return;
        }
        let Some(address) = airpods_address() else {
            return;
        };
        let o = self.observe(address).await;
        let decision = self.policy.update(&o);
        match decision.action {
            Action::None => {
                if !decision.reason.is_empty() {
                    info!("Not connecting the AirPods: {}", decision.reason);
                }
            }
            Action::Connect => {
                info!("Connecting the AirPods: {}", decision.reason);
                self.connect(address, true).await;
            }
            Action::Release => {
                info!("Releasing the AirPods: {}", decision.reason);
                bluetoothctl("disconnect", address).await;
            }
            Action::HandOver => {
                info!("{}", decision.reason);
                // Chromium left playing after losing the AirPods pauses itself and resumes about
                // 30 s later, which would pull them straight back. Paused over MPRIS it stays paused.
                self.handed_over_players = pause_players().await;
                let body = if self.handed_over_players.is_empty() {
                    "Press Move back to switch them to this computer."
                } else {
                    "Playback here is paused. Press Move back to switch them to this computer."
                };
                notify("AirPods moved away", body, Some(self.move_back_tx.clone()));
            }
        }
    }

    /// Runs inside the poll loop, so no observation is made while it is in flight and its
    /// own pause and resume aren't read as the user.
    async fn connect(&mut self, address: Address, hold_playback: bool) {
        // Held until the AirPods are ready, so it doesn't start on the speakers. Sounds that
        // aren't MPRIS players, like notifications, can't be held.
        let held = if hold_playback { pause_players().await } else { Vec::new() };
        let mut connected = false;
        for attempt in 1..=CONNECT_ATTEMPTS {
            if bluetoothctl("connect", address).await {
                connected = true;
                break;
            }
            if attempt < CONNECT_ATTEMPTS {
                info!("AirPods connect attempt {} failed, retrying", attempt);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        if connected {
            let own = sink_prefix(address);
            let deadline = Instant::now() + SINK_WAIT;
            while !blocking(sink_names_sync).await.unwrap_or_default().iter().any(|s| s.starts_with(&own)) {
                if Instant::now() > deadline {
                    warn!("AirPods connected, but their audio output didn't appear");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            self.policy.connect_succeeded();
            info!("AirPods connected");
            notify("AirPods connected to this computer", "", None);
        } else {
            info!("Could not connect the AirPods after {} attempts (in the case or out of range?)", CONNECT_ATTEMPTS);
            self.policy.connect_failed(now_ms());
        }
        resume_players(held).await;
    }

    async fn move_back(&mut self) {
        info!("Move back pressed");
        if !self.handed_over_players.is_empty() {
            // Playback starting brings them back through the normal connect path.
            resume_players(std::mem::take(&mut self.handed_over_players)).await;
        } else if let Some(address) = airpods_address() {
            self.connect(address, false).await;
        }
    }
}

pub async fn run(adapter: Adapter) {
    let (move_back_tx, mut move_back_rx) = mpsc::unbounded_channel();
    let mut controller = Controller {
        adapter,
        policy: SwitchPolicy::default(),
        enabled: false,
        handed_over_players: Vec::new(),
        move_back_tx,
    };
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => controller.evaluate().await,
            Some(()) = move_back_rx.recv() => controller.move_back().await,
        }
    }
}
