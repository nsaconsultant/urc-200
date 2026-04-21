//! urc200-server — Axum HTTP + WebSocket host for the URC-200.
//!
//! MVP scope (S-100): serve a placeholder UI, stream typed telemetry events
//! over a single WebSocket. PTT arbiter, audio path, and operator/maintainer
//! workflows land in subsequent stories.
//!
//! Safety invariants enforced here (per Murat's revised risk matrix):
//!   - **Startup unkey:** we send `E` (receive mode) unconditionally before
//!     accepting any client connection. Assumes prior instance may have died
//!     keyed — the first obligation is "put the radio into RX."
//!   - **Shutdown unkey:** SIGTERM / SIGINT fires `E` before the serial port
//!     is dropped, waits for the write to flush (≤100 ms), and only then
//!     exits.

mod audio;
mod channels;
mod commands;
mod control_ws;
mod db;
mod dsp;
mod ptt;
mod scan;
#[cfg(feature = "sdr")]
mod sdr_routes;

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal;
use tokio::sync::broadcast;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{info, warn};
use urc200_proto::{
    LampLevel, Mode, ModMode, OpCommand, PowerLevel, Rssi, SquelchStatus, SynthLock, TextMode,
};
use urc200_serial::{
    Poller, Radio, SerialConfig, SerialTransport, TelemetryUpdate, DEFAULT_TIMEOUT,
};

use crate::audio::{AudioCapture, AudioTx};
use crate::db::Db;
use crate::ptt::PttHandle;
use crate::scan::{ScanConfig, ScannerHandle};

#[derive(Clone)]
struct AppState {
    radio: Radio,
    telemetry: broadcast::Sender<TelemetryUpdate>,
    ptt: PttHandle,
    db: Db,
    audio: AudioCapture,
    audio_tx: AudioTx,
    scanner: ScannerHandle,
    #[cfg(feature = "sdr")]
    sdr: sdr_routes::SdrHandle,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "urc200_server=info,urc200_serial=info,tower_http=info".into()),
        )
        .init();

    let port = std::env::var("URC_PORT")
        .ok()
        .or_else(autodetect_serial_port)
        .unwrap_or_else(|| "/dev/ttyUSB0".to_string());
    let bind: SocketAddr = std::env::var("URC_BIND")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()
        .context("URC_BIND must be ip:port")?;
    let static_dir: PathBuf = std::env::var("URC_STATIC")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Resolve the static/ directory alongside this crate at compile time.
            let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            crate_dir.join("static")
        });
    let db_path: PathBuf = std::env::var("URC_DB")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/urc200/channels.db"));

    info!(port = %port, bind = %bind, static_dir = %static_dir.display(), db = %db_path.display(), "starting urc200-server");

    let database = Db::open(&db_path).context("open channels.db")?;

    let audio_device = std::env::var("URC_AUDIO").unwrap_or_else(|_| "hw:UAA2,0".to_string());
    let audio = AudioCapture::spawn(audio_device.clone());
    let audio_tx = AudioTx::spawn(audio_device);

    // Open the serial port and bring up the dispatcher.
    let cfg = SerialConfig::urc200(&port);
    let transport = SerialTransport::open(&cfg).with_context(|| format!("open {port}"))?;
    let radio_handle = Radio::spawn(transport, DEFAULT_TIMEOUT);

    // Startup invariant: unkey, whatever state the radio was in.
    // Best-effort — if the radio isn't powered yet, we still proceed so the
    // UI can show the disconnection and the operator can power it on.
    match radio_handle.radio.send(OpCommand::Receive).await {
        Ok(resp) => info!(?resp, "startup E sent"),
        Err(e) => warn!(error = ?e, "startup E failed — radio may be off; continuing"),
    }

    // Spawn the telemetry poller.
    let poller = Poller::spawn(radio_handle.radio.clone());

    // Spawn the PTT arbiter. Holds ownership state; watchdogs heartbeat.
    let (ptt_handle, ptt_task) = PttHandle::spawn(radio_handle.radio.clone());

    // Spawn the channel-library scanner.
    let (scanner_handle, scanner_task) = ScannerHandle::spawn(
        radio_handle.radio.clone(),
        database.clone(),
        poller.sender(),
    );

    #[cfg(feature = "sdr")]
    let sdr = {
        let cfg = radio_sdr::SdrConfig {
            device_args: std::env::var("URC_SDR_DEVICE")
                .unwrap_or_else(|_| "driver=sdrplay".into()),
            center_hz: std::env::var("URC_SDR_CENTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(251_950_000),
            ..Default::default()
        };
        info!(device = %cfg.device_args, center = cfg.center_hz, "starting SDR capture");
        sdr_routes::SdrHandle::spawn(cfg)
    };

    let state = AppState {
        radio: radio_handle.radio.clone(),
        telemetry: poller.sender(),
        ptt: ptt_handle.clone(),
        db: database,
        audio: audio.clone(),
        audio_tx: audio_tx.clone(),
        scanner: scanner_handle.clone(),
        #[cfg(feature = "sdr")]
        sdr,
    };

    let mut app = Router::new()
        .route("/api/health", get(health))
        .route("/api/ws/telemetry", get(ws_telemetry))
        .route("/api/ws/control", get(ws_control))
        .route("/api/ws/audio/rx", get(ws_audio_rx))
        .route("/api/ws/audio/tx", get(ws_audio_tx))
        .route("/api/ws/scan", get(ws_scan))
        .route("/api/tx/ctcss", get(get_ctcss).post(set_ctcss))
        .route("/api/audio/filters/rx", get(get_rx_filters).post(set_rx_filters))
        .route("/api/audio/filters/tx", get(get_tx_filters).post(set_tx_filters))
        .route("/api/features", get(features))
        .nest("/api/command", commands::router())
        .nest("/api/channels", channels::router());

    #[cfg(feature = "sdr")]
    {
        app = app.merge(sdr_routes::router());
    }

    let app = app
        .fallback_service(ServeDir::new(&static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await.context("bind")?;
    info!(listening_on = %bind, "ready");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;

    info!("shutdown signal received; flushing E and closing");

    // Stop the scanner first so it doesn't retune the radio mid-shutdown.
    scanner_handle.shutdown().await;
    let _ = scanner_task.await;

    // PTT arbiter next (it'll emit E if held), then explicit E for good
    // measure, then poller, then the radio dispatcher itself.
    ptt_handle.shutdown().await;
    let _ = ptt_task.await;

    match tokio::time::timeout(
        Duration::from_millis(500),
        radio_handle.radio.send(OpCommand::Receive),
    )
    .await
    {
        Ok(Ok(_)) => info!("shutdown E acked"),
        Ok(Err(e)) => warn!(error = ?e, "shutdown E failed"),
        Err(_) => warn!("shutdown E timed out"),
    }

    poller.shutdown().await;
    radio_handle.shutdown().await;
    info!("clean exit");
    Ok(())
}

async fn ws_control(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| control_ws::handle(socket, state.ptt))
        .into_response()
}

async fn ws_audio_rx(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    let rx = state.audio.subscribe();
    let rate = state.audio.actual_rate();
    ws.on_upgrade(move |socket| handle_audio_rx(socket, rx, rate))
        .into_response()
}

async fn ws_audio_tx(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_audio_tx(socket, state.audio_tx, state.ptt))
        .into_response()
}

#[derive(Serialize)]
struct CtcssState {
    enabled: bool,
    freq_hz: f32,
    amplitude: u16,
}

#[derive(serde::Deserialize)]
struct CtcssReq {
    /// Null or omitted disables CTCSS. A value in 30-300 Hz enables it.
    freq_hz: Option<f32>,
    /// Optional amplitude in i16 units (0-32767); default 3200 (~10% peak).
    amplitude: Option<u16>,
}

async fn get_ctcss(State(s): State<AppState>) -> Json<CtcssState> {
    let (enabled, freq_hz, amplitude) = s.audio_tx.ctcss.get();
    Json(CtcssState {
        enabled,
        freq_hz,
        amplitude,
    })
}

async fn set_ctcss(State(s): State<AppState>, Json(req): Json<CtcssReq>) -> Json<CtcssState> {
    s.audio_tx.ctcss.set(req.freq_hz);
    if let Some(a) = req.amplitude {
        s.audio_tx.ctcss.set_amplitude(a);
    }
    let (enabled, freq_hz, amplitude) = s.audio_tx.ctcss.get();
    Json(CtcssState {
        enabled,
        freq_hz,
        amplitude,
    })
}

#[derive(serde::Deserialize)]
struct FilterReq {
    hp_enabled: Option<bool>,
    lp_enabled: Option<bool>,
    gate_enabled: Option<bool>,
    hp_fc: Option<f32>,
    lp_fc: Option<f32>,
    gate_db: Option<f32>,
}

fn apply_filter_req(cfg: &dsp::FilterConfig, req: FilterReq) {
    use std::sync::atomic::Ordering;
    let mut changed = false;
    if let Some(v) = req.hp_enabled { cfg.hp_enabled.store(v, Ordering::Relaxed); changed = true; }
    if let Some(v) = req.lp_enabled { cfg.lp_enabled.store(v, Ordering::Relaxed); changed = true; }
    if let Some(v) = req.gate_enabled { cfg.gate_enabled.store(v, Ordering::Relaxed); changed = true; }
    if let Some(v) = req.hp_fc {
        cfg.hp_fc.store((v * 10.0).round().clamp(200.0, 80_000.0) as u32, Ordering::Relaxed);
        changed = true;
    }
    if let Some(v) = req.lp_fc {
        cfg.lp_fc.store((v * 10.0).round().clamp(1_000.0, 200_000.0) as u32, Ordering::Relaxed);
        changed = true;
    }
    if let Some(v) = req.gate_db {
        // gate_db stored as -dB × 10. e.g. -40 dB -> 400
        let abs = (-v).abs();
        cfg.gate_db.store((abs * 10.0).round() as u32, Ordering::Relaxed);
        changed = true;
    }
    if changed { cfg.bump(); }
}

async fn get_rx_filters(State(s): State<AppState>) -> Json<dsp::FilterSnapshot> { Json(s.audio.filters.snapshot()) }
async fn set_rx_filters(State(s): State<AppState>, Json(req): Json<FilterReq>) -> Json<dsp::FilterSnapshot> {
    apply_filter_req(&s.audio.filters, req);
    Json(s.audio.filters.snapshot())
}
async fn get_tx_filters(State(s): State<AppState>) -> Json<dsp::FilterSnapshot> { Json(s.audio_tx.filters.snapshot()) }
async fn set_tx_filters(State(s): State<AppState>, Json(req): Json<FilterReq>) -> Json<dsp::FilterSnapshot> {
    apply_filter_req(&s.audio_tx.filters, req);
    Json(s.audio_tx.filters.snapshot())
}

// -------- Scanner WS --------

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ScanClientMsg {
    Start(ScanConfig),
    Stop,
    Skip,
}

async fn ws_scan(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_scan(socket, state.scanner))
        .into_response()
}

async fn handle_scan(mut socket: WebSocket, scanner: ScannerHandle) {
    use tokio::sync::broadcast::error::RecvError;
    // IMPORTANT: subscribe BEFORE reading the snapshot so we don't race a
    // stop that happens between the two — if stop fires after we read the
    // snapshot but before we subscribe, we'd miss the Stopped event and
    // display a phantom scan. Subscribing first means any event emitted
    // after the snapshot read is guaranteed to arrive.
    let mut events = scanner.subscribe();

    // Catch up the new client with whatever's currently running. Broadcast
    // doesn't replay history, so without this a browser that joins mid-scan
    // sits on its idle "Start" button while another tab is actively scanning.
    if let Some(snap) = scanner.current() {
        let started = serde_json::json!({
            "type": "started",
            "total": snap.total,
            "group": snap.group,
        });
        if socket.send(Message::Text(started.to_string())).await.is_err() {
            return;
        }
        if let Some(tuned) = &snap.last_tuned {
            let json = serde_json::to_string(tuned).unwrap_or_default();
            if socket.send(Message::Text(json)).await.is_err() {
                return;
            }
        }
    }
    info!("scan ws client connected");
    loop {
        tokio::select! {
            biased;
            ev = events.recv() => match ev {
                Ok(e) => {
                    let json = serde_json::to_string(&e).unwrap_or_default();
                    if socket.send(Message::Text(json)).await.is_err() { break; }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                None => break,
                Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ScanClientMsg>(&text) {
                        Ok(ScanClientMsg::Start(cfg)) => {
                            match scanner.start(cfg).await {
                                Ok(total) => {
                                    let reply = serde_json::json!({ "type": "start_ack", "total": total });
                                    let _ = socket.send(Message::Text(reply.to_string())).await;
                                }
                                Err(e) => {
                                    let reply = serde_json::json!({ "type": "start_err", "message": e });
                                    let _ = socket.send(Message::Text(reply.to_string())).await;
                                }
                            }
                        }
                        Ok(ScanClientMsg::Stop) => scanner.stop().await,
                        Ok(ScanClientMsg::Skip) => scanner.skip().await,
                        Err(e) => {
                            let reply = serde_json::json!({ "type": "error", "message": format!("bad json: {e}") });
                            let _ = socket.send(Message::Text(reply.to_string())).await;
                        }
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    }
    info!("scan ws client disconnected");
}

/// Receive mic audio from the browser and forward to the UAA playback PCM.
/// Only bytes arriving while this connection's corresponding browser holds
/// PTT should be sent — the browser enforces that client-side. The server
/// writes whatever arrives; even if a stray frame lands after release, the
/// radio isn't keyed so the UAA playback goes nowhere on-air.
async fn handle_audio_tx(mut socket: WebSocket, audio_tx: AudioTx, _ptt: PttHandle) {
    // Tell the client what rate the server expects.
    let hdr = serde_json::json!({
        "type": "audio_tx_header",
        "sample_rate": if audio_tx.actual_rate() > 0 { audio_tx.actual_rate() } else { audio::PREFERRED_SAMPLE_RATE },
        "channels": audio::CHANNELS,
        "format": "s16le",
    });
    if socket.send(Message::Text(hdr.to_string())).await.is_err() {
        return;
    }
    tracing::info!("audio tx client connected");
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Binary(bytes) => {
                if bytes.len() % 2 != 0 || bytes.is_empty() {
                    continue;
                }
                let samples: Vec<i16> = bytes
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let _ = audio_tx.try_send(samples);
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    tracing::info!("audio tx client disconnected");
}

async fn handle_audio_rx(
    socket: WebSocket,
    rx: broadcast::Receiver<std::sync::Arc<Vec<i16>>>,
    actual_rate: u32,
) {
    use tokio::sync::broadcast::error::RecvError;
    let mut socket = socket;
    let mut rx = rx;
    // Send a tiny header frame so the client learns sample rate + channels.
    let hdr = serde_json::json!({
        "type": "audio_header",
        "sample_rate": if actual_rate > 0 { actual_rate } else { audio::PREFERRED_SAMPLE_RATE },
        "channels": audio::CHANNELS,
        "format": "s16le",
    });
    if socket
        .send(Message::Text(hdr.to_string()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        match rx.recv().await {
            Ok(samples) => {
                // Convert to little-endian bytes.
                let mut bytes = Vec::with_capacity(samples.len() * 2);
                for s in samples.iter() {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                if socket.send(Message::Binary(bytes)).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }
}

/// Walk common USB-serial device paths and return the first one whose node
/// actually exists. Lets us survive PL2303 re-enumeration (ttyUSB0 → ttyUSB1)
/// without hand-editing config.
fn autodetect_serial_port() -> Option<String> {
    for path in [
        "/dev/ttyUSB0",
        "/dev/ttyUSB1",
        "/dev/ttyUSB2",
        "/dev/ttyUSB3",
        "/dev/ttyACM0",
        "/dev/ttyACM1",
    ] {
        if std::fs::metadata(path).is_ok() {
            return Some(path.to_string());
        }
    }
    None
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        let mut s = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("register SIGTERM handler");
        s.recv().await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => info!("SIGINT"),
        _ = term => info!("SIGTERM"),
    }
}

// --------- Handlers ---------

async fn health(State(_state): State<AppState>) -> Json<HealthPayload> {
    Json(HealthPayload {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct FeaturesPayload {
    sdr: bool,
}

async fn features(State(_state): State<AppState>) -> Json<FeaturesPayload> {
    Json(FeaturesPayload {
        sdr: cfg!(feature = "sdr"),
    })
}

async fn ws_telemetry(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws_telemetry(socket, state))
        .into_response()
}

async fn handle_ws_telemetry(mut socket: WebSocket, state: AppState) {
    let mut rx = state.telemetry.subscribe();
    info!("ws telemetry client connected");
    loop {
        tokio::select! {
            // Drain the broadcast, forwarding to the socket.
            msg = rx.recv() => match msg {
                Ok(update) => {
                    let evt = WsEvent::from(update);
                    let json = match serde_json::to_string(&evt) {
                        Ok(s) => s,
                        Err(e) => { warn!(error = ?e, "json encode"); continue; }
                    };
                    if socket.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let _ = socket
                        .send(Message::Text(
                            serde_json::to_string(&WsEvent::Lagged { dropped: n }).unwrap(),
                        ))
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Read side: drop the socket on any client-initiated close.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => {}, // ignore pings / text from client for now
            },
        }
    }
    info!("ws telemetry client disconnected");
}

// --------- Wire types ---------

#[derive(Serialize)]
struct HealthPayload {
    status: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsEvent {
    Rssi {
        value: u8,
    },
    Squelch {
        broken: bool,
    },
    SynthLock {
        locked: bool,
    },
    Mode {
        mode: &'static str,
    },
    General {
        text_mode: &'static str,
        speaker_on: bool,
        lamp: &'static str,
        overtemp: bool,
        options_byte: u8,
        ebn30: bool,
        ebn400: bool,
    },
    Preset {
        preset: u8,
        tx_mhz: f64,
        rx_mhz: f64,
        mod_tx_rx: &'static str,
        mod_tx_only: &'static str,
        power: &'static str,
        on_scan_list: bool,
    },
    Error {
        inquiry: String,
        kind: String,
    },
    Lagged {
        dropped: u64,
    },
}

impl From<TelemetryUpdate> for WsEvent {
    fn from(u: TelemetryUpdate) -> Self {
        match u {
            TelemetryUpdate::Rssi(Rssi(value)) => WsEvent::Rssi { value },
            TelemetryUpdate::Squelch(s) => WsEvent::Squelch {
                broken: matches!(s, SquelchStatus::Broken),
            },
            TelemetryUpdate::SynthLock(SynthLock(locked)) => WsEvent::SynthLock { locked },
            TelemetryUpdate::Mode(m) => WsEvent::Mode {
                mode: match m {
                    Mode::Receive => "receive",
                    Mode::Transmit => "transmit",
                    Mode::Beacon => "beacon",
                },
            },
            TelemetryUpdate::General(g) => {
                let ebn30 = g.has_ebn30();
                let ebn400 = g.has_ebn400();
                WsEvent::General {
                    text_mode: text_mode_str(g.text_mode),
                    speaker_on: g.speaker_on,
                    lamp: lamp_str(g.lamp),
                    overtemp: g.overtemp,
                    options_byte: g.options_byte,
                    ebn30,
                    ebn400,
                }
            }
            TelemetryUpdate::Preset(p) => WsEvent::Preset {
                preset: p.preset.get(),
                tx_mhz: p.tx_hz as f64 / 1_000_000.0,
                rx_mhz: p.rx_hz as f64 / 1_000_000.0,
                mod_tx_rx: mod_str(p.mod_tx_rx),
                mod_tx_only: mod_str(p.mod_tx_only),
                power: power_str(p.power),
                on_scan_list: p.on_scan_list,
            },
            TelemetryUpdate::Error { inquiry, kind } => WsEvent::Error {
                inquiry: format!("{inquiry:?}"),
                kind: format!("{kind:?}"),
            },
        }
    }
}

fn text_mode_str(t: TextMode) -> &'static str {
    match t {
        TextMode::Pt => "pt",
        TextMode::Ct => "ct",
    }
}

fn lamp_str(l: LampLevel) -> &'static str {
    match l {
        LampLevel::Off => "off",
        LampLevel::Lo => "lo",
        LampLevel::Med => "med",
        LampLevel::Hi => "hi",
    }
}

fn mod_str(m: ModMode) -> &'static str {
    match m {
        ModMode::Am => "am",
        ModMode::Fm => "fm",
    }
}

fn power_str(p: PowerLevel) -> &'static str {
    match p {
        PowerLevel::Lo => "lo",
        PowerLevel::Med => "med",
        PowerLevel::Hi => "hi",
    }
}

