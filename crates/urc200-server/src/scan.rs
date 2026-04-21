//! Library scanner: walks a channel group, tunes each one, watches the
//! poller's squelch telemetry to detect activity, dwells on hits, moves on
//! when the frequency goes quiet. Pure RX — never keys the transmitter.
//!
//! The operator picks a group and a dwell time; the scanner runs as its own
//! actor, emits events on a broadcast channel, and responds to Stop / Skip
//! commands over an mpsc. One client feeds control + event stream through
//! `/api/ws/scan`.

use crate::db::{Channel, Db};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};
use urc200_proto::{Band, Freq, ModMode, OpCommand, SquelchStatus, Step};
use urc200_serial::{Radio, TelemetryUpdate};

/// What the client asks the scanner to do.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScanConfig {
    /// None = scan all groups; Some("Aviation") = scan that group only.
    pub group: Option<String>,
    /// How long to stay on an active channel after squelch closes.
    /// Default 3000 ms gives the operator time to hear the end of a transmission.
    #[serde(default = "default_dwell")]
    pub dwell_ms: u64,
    /// How long to wait after tuning before reading squelch (let the radio settle).
    #[serde(default = "default_settle")]
    pub settle_ms: u64,
    /// If true, stop the scan entirely on the first hit (instead of just dwelling).
    #[serde(default)]
    pub stop_on_hit: bool,
}

fn default_dwell() -> u64 {
    3000
}
fn default_settle() -> u64 {
    200
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScanEvent {
    Started {
        total: usize,
        group: Option<String>,
    },
    Tuned {
        channel_id: i64,
        name: String,
        rx_mhz: f64,
        tx_mhz: f64,
        idx: usize,
        total: usize,
    },
    Hit {
        channel_id: i64,
        name: String,
        rx_mhz: f64,
    },
    Stopped {
        reason: String,
    },
}

/// Point-in-time snapshot of what the scanner is doing. Kept in a shared
/// slot on [`ScannerHandle`] so a browser opening the `/api/ws/scan` socket
/// after a scan has already started can be caught up with a synthetic
/// `Started` + most-recent `Tuned` event, instead of sitting on its idle
/// "Start" button while another tab is actively running.
#[derive(Debug, Clone)]
pub struct ScanSnapshot {
    pub total: usize,
    pub group: Option<String>,
    pub last_tuned: Option<ScanEvent>, // always a Tuned variant when Some
}

enum ScanCmd {
    Start {
        config: ScanConfig,
        reply: oneshot::Sender<Result<usize, String>>,
    },
    Stop,
    Skip,
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct ScannerHandle {
    cmd: mpsc::Sender<ScanCmd>,
    events: broadcast::Sender<ScanEvent>,
    snapshot: Arc<Mutex<Option<ScanSnapshot>>>,
}

impl ScannerHandle {
    pub fn spawn(
        radio: Radio,
        db: Db,
        telemetry: broadcast::Sender<TelemetryUpdate>,
    ) -> (Self, JoinHandle<()>) {
        let (cmd, rx) = mpsc::channel(32);
        let (events, _) = broadcast::channel(64);
        let snapshot = Arc::new(Mutex::new(None::<ScanSnapshot>));
        let ev = events.clone();
        let snap = snapshot.clone();
        let task = tokio::spawn(actor(rx, radio, db, telemetry, ev, snap));
        (Self { cmd, events, snapshot }, task)
    }

    /// Current scan state, if any. Used by the WS handler to catch up a
    /// newly-connected client.
    pub fn current(&self) -> Option<ScanSnapshot> {
        self.snapshot.lock().ok().and_then(|g| g.clone())
    }

    pub async fn start(&self, config: ScanConfig) -> Result<usize, String> {
        let (reply, rx) = oneshot::channel();
        if self.cmd.send(ScanCmd::Start { config, reply }).await.is_err() {
            return Err("scanner actor closed".into());
        }
        rx.await.unwrap_or_else(|_| Err("reply dropped".into()))
    }

    pub async fn stop(&self) {
        let _ = self.cmd.send(ScanCmd::Stop).await;
    }

    pub async fn skip(&self) {
        let _ = self.cmd.send(ScanCmd::Skip).await;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ScanEvent> {
        self.events.subscribe()
    }

    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.cmd.send(ScanCmd::Shutdown(reply)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    NeedsTune,
    Settling,
    Dwelling,
}

struct ScanState {
    config: ScanConfig,
    channels: Vec<Channel>,
    idx: usize,
    phase: Phase,
    phase_started: Instant,
}

async fn actor(
    mut rx: mpsc::Receiver<ScanCmd>,
    radio: Radio,
    db: Db,
    telemetry: broadcast::Sender<TelemetryUpdate>,
    events: broadcast::Sender<ScanEvent>,
    snapshot: Arc<Mutex<Option<ScanSnapshot>>>,
) {
    let mut state: Option<ScanState> = None;
    let mut squelch_rx = telemetry.subscribe();
    let mut latest_squelch = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Scoped helpers so we keep the mutex critical section tiny.
    let set_snapshot = |s: Option<ScanSnapshot>| {
        if let Ok(mut g) = snapshot.lock() {
            *g = s;
        }
    };
    let update_last_tuned = |ev: &ScanEvent| {
        if let Ok(mut g) = snapshot.lock() {
            if let Some(snap) = g.as_mut() {
                snap.last_tuned = Some(ev.clone());
            }
        }
    };

    loop {
        tokio::select! {
            biased;
            maybe = rx.recv() => match maybe {
                None => break,
                Some(ScanCmd::Start { config, reply }) => {
                    match load_channels(&db, &config).await {
                        Ok(chs) if !chs.is_empty() => {
                            let total = chs.len();
                            info!(group = ?config.group, total, dwell_ms = config.dwell_ms, "scan starting");
                            let _ = events.send(ScanEvent::Started { total, group: config.group.clone() });
                            set_snapshot(Some(ScanSnapshot {
                                total,
                                group: config.group.clone(),
                                last_tuned: None,
                            }));
                            state = Some(ScanState {
                                config,
                                channels: chs,
                                idx: 0,
                                phase: Phase::NeedsTune,
                                phase_started: Instant::now(),
                            });
                            let _ = reply.send(Ok(total));
                        }
                        Ok(_) => {
                            let _ = reply.send(Err("no tunable channels in that group".into()));
                            let _ = events.send(ScanEvent::Stopped {
                                reason: "empty_list".into(),
                            });
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e.clone()));
                            let _ = events.send(ScanEvent::Stopped { reason: e });
                        }
                    }
                }
                Some(ScanCmd::Stop) => {
                    if state.is_some() {
                        info!("scan stopped by user");
                        let _ = events.send(ScanEvent::Stopped { reason: "user_stop".into() });
                        state = None;
                        set_snapshot(None);
                    }
                }
                Some(ScanCmd::Skip) => {
                    if let Some(s) = &mut state {
                        s.idx = (s.idx + 1) % s.channels.len();
                        s.phase = Phase::NeedsTune;
                        s.phase_started = Instant::now();
                    }
                }
                Some(ScanCmd::Shutdown(reply)) => {
                    state = None;
                    set_snapshot(None);
                    let _ = reply.send(());
                    break;
                }
            },
            result = squelch_rx.recv() => match result {
                Ok(TelemetryUpdate::Squelch(s)) => latest_squelch = matches!(s, SquelchStatus::Broken),
                Ok(_) => {},
                Err(_) => {},
            },
            _ = ticker.tick() => {
                if let Some(s) = &mut state {
                    let (done, fresh_tuned) = step(s, &radio, latest_squelch, &events).await;
                    if let Some(ev) = fresh_tuned {
                        update_last_tuned(&ev);
                    }
                    if done {
                        state = None;
                        set_snapshot(None);
                    }
                }
            }
        }
    }
}

async fn load_channels(db: &Db, config: &ScanConfig) -> Result<Vec<Channel>, String> {
    let all = db
        .list_channels(config.group.clone())
        .await
        .map_err(|e| format!("db: {e}"))?;
    // Skip channels that can't be tuned at all (no band match for either RX or TX).
    Ok(all
        .into_iter()
        .filter(|c| band_for(c.rx_hz).is_some() || band_for(c.tx_hz).is_some())
        .collect())
}

fn band_for(hz: u32) -> Option<Band> {
    match hz {
        30_000_000..=90_000_000 => Some(Band::Lvhf),
        115_000_000..=173_995_000 | 225_000_000..=399_995_000 => Some(Band::Base),
        400_000_000..=420_000_000 => Some(Band::Uhf400),
        _ => None,
    }
}

fn mode_to_urc(mode: &str) -> Option<ModMode> {
    match mode {
        "am" => Some(ModMode::Am),
        "fm" => Some(ModMode::Fm),
        _ => None,
    }
}

/// Drives one tick of the state machine. Returns `(done, fresh_tuned)`
/// where `done` is true when the scan is finished (stop-on-hit fired), and
/// `fresh_tuned` carries the `Tuned` event if the scanner just moved to a
/// new channel (so callers can update the cross-client state snapshot).
async fn step(
    s: &mut ScanState,
    radio: &Radio,
    latest_squelch: bool,
    events: &broadcast::Sender<ScanEvent>,
) -> (bool, Option<ScanEvent>) {
    let now = Instant::now();
    match s.phase {
        Phase::NeedsTune => {
            let ch = &s.channels[s.idx];
            if let Err(e) = tune_for_scan(radio, ch).await {
                warn!(name = %ch.name, error = ?e, "scan tune failed");
                // Advance past the problem channel rather than getting stuck.
                s.idx = (s.idx + 1) % s.channels.len();
                s.phase = Phase::NeedsTune;
                s.phase_started = now;
                return (false, None);
            }
            let tuned = ScanEvent::Tuned {
                channel_id: ch.id,
                name: ch.name.clone(),
                rx_mhz: ch.rx_hz as f64 / 1_000_000.0,
                tx_mhz: ch.tx_hz as f64 / 1_000_000.0,
                idx: s.idx,
                total: s.channels.len(),
            };
            let _ = events.send(tuned.clone());
            s.phase = Phase::Settling;
            s.phase_started = now;
            return (false, Some(tuned));
        }
        Phase::Settling => {
            if now.duration_since(s.phase_started) >= Duration::from_millis(s.config.settle_ms) {
                if latest_squelch {
                    let ch = &s.channels[s.idx];
                    let _ = events.send(ScanEvent::Hit {
                        channel_id: ch.id,
                        name: ch.name.clone(),
                        rx_mhz: ch.rx_hz as f64 / 1_000_000.0,
                    });
                    if s.config.stop_on_hit {
                        let _ = events.send(ScanEvent::Stopped {
                            reason: format!("first_hit:{}", ch.name),
                        });
                        return (true, None);
                    }
                    s.phase = Phase::Dwelling;
                    s.phase_started = now;
                } else {
                    s.idx = (s.idx + 1) % s.channels.len();
                    s.phase = Phase::NeedsTune;
                    s.phase_started = now;
                }
            }
        }
        Phase::Dwelling => {
            // While squelch is still open, keep dwelling (reset the timer).
            if latest_squelch {
                s.phase_started = now;
            } else if now.duration_since(s.phase_started) >= Duration::from_millis(s.config.dwell_ms) {
                // Quiet for dwell_ms — advance.
                s.idx = (s.idx + 1) % s.channels.len();
                s.phase = Phase::NeedsTune;
                s.phase_started = now;
            }
        }
    }
    (false, None)
}

async fn tune_for_scan(radio: &Radio, ch: &Channel) -> Result<(), String> {
    // Pick a band that contains at least the RX (TX unreachable for OOB is fine;
    // we're not going to key anyway).
    let band = band_for(ch.rx_hz)
        .or_else(|| band_for(ch.tx_hz))
        .ok_or_else(|| "out of band".to_string())?;
    let step = Step::Khz5;
    let rx = Freq::new(ch.rx_hz, band, step).map_err(|e| format!("rx: {e}"))?;
    radio
        .send(OpCommand::SetRx(rx))
        .await
        .map_err(|e| format!("rx send: {e}"))?;
    // Also set TX so a rapid operator PTT doesn't transmit on the previous freq.
    // If TX is OOB, fall back to simplex on RX.
    let tx_hz = if band_for(ch.tx_hz).is_some() { ch.tx_hz } else { ch.rx_hz };
    let tx = Freq::new(tx_hz, band, step).map_err(|e| format!("tx: {e}"))?;
    radio
        .send(OpCommand::SetTx(tx))
        .await
        .map_err(|e| format!("tx send: {e}"))?;
    // Apply mode if the channel has one.
    if let Some(m) = ch.mode.as_deref().and_then(mode_to_urc) {
        radio
            .send(OpCommand::ModTxRx(m))
            .await
            .map_err(|e| format!("mode send: {e}"))?;
    }
    Ok(())
}
