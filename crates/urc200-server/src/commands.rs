//! HTTP command endpoints for non-transmit radio control.
//!
//! All routes return `Json<CommandReply>`. HTTP status is 200 on radio ACK,
//! 409 on radio NAK, 503 on transport / dispatcher error, 400 on bad input.
//!
//! **Hard-refused at the HTTP layer** (per user standing rule: no transmit
//! from code): `B`, `*1`, `I`, `K`. These commands are absent from this
//! module and no other HTTP route produces them.

use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use urc200_proto::{
    Band, Freq, LampLevel, ModMode, OpCommand, PowerLevel, PresetId, Step, TextMode,
};
use urc200_serial::RadioError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/preset/:n", post(post_preset))
        .route("/lamp/:level", post(post_lamp))
        .route("/speaker/:state", post(post_speaker))
        .route("/squelch/:v", post(post_squelch))
        .route("/mod/:m", post(post_mod))
        .route("/mod_tx_only/:m", post(post_mod_tx_only))
        .route("/text/:t", post(post_text))
        .route("/power/:p", post(post_power))
        .route("/scan/:state", post(post_scan))
        .route("/scan_list_member/:state", post(post_scan_list))
        .route("/store", post(post_store))
        .route("/tune", post(post_tune))
}

#[derive(Serialize)]
pub struct CommandReply {
    pub ok: bool,
    pub response_kind: &'static str,
    pub data: Option<String>,
}

impl CommandReply {
    fn from_response(r: urc200_proto::Response) -> (StatusCode, Self) {
        let data = if r.data().is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(r.data()).into_owned())
        };
        if r.is_nak() {
            (
                StatusCode::CONFLICT,
                Self {
                    ok: false,
                    response_kind: "nak",
                    data,
                },
            )
        } else if r.is_ht() {
            (
                StatusCode::OK,
                Self {
                    ok: true,
                    response_kind: "ht",
                    data,
                },
            )
        } else {
            (
                StatusCode::OK,
                Self {
                    ok: true,
                    response_kind: "ack",
                    data,
                },
            )
        }
    }
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

async fn dispatch(
    state: &AppState,
    cmd: OpCommand,
) -> Result<(StatusCode, Json<CommandReply>), (StatusCode, String)> {
    match state.radio.send(cmd).await {
        Ok(resp) => {
            let (code, body) = CommandReply::from_response(resp);
            Ok((code, Json(body)))
        }
        Err(RadioError::Timeout(d)) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("radio timeout after {d:?}"),
        )),
        Err(RadioError::Fault) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "radio protocol fault (3 NAKs)".into(),
        )),
        Err(RadioError::Transport(e)) => {
            Err((StatusCode::SERVICE_UNAVAILABLE, format!("transport: {e}")))
        }
        Err(RadioError::Closed) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "radio handle closed".into(),
        )),
    }
}

// ---- Route handlers ----

async fn post_preset(
    State(s): State<AppState>,
    Path(n): Path<u8>,
) -> Response {
    let Some(p) = PresetId::new(n) else {
        return bad("preset must be 0..=9");
    };
    match dispatch(&s, OpCommand::Preset(p)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_lamp(
    State(s): State<AppState>,
    Path(level): Path<String>,
) -> Response {
    let l = match level.to_lowercase().as_str() {
        "off" => LampLevel::Off,
        "lo" | "low" | "1" => LampLevel::Lo,
        "med" | "medium" | "2" => LampLevel::Med,
        "hi" | "high" | "3" => LampLevel::Hi,
        _ => return bad("lamp must be off/lo/med/hi"),
    };
    match dispatch(&s, OpCommand::Lamp(l)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_speaker(
    State(s): State<AppState>,
    Path(state): Path<String>,
) -> Response {
    let on = parse_on_off(&state).unwrap_or(None);
    let Some(on) = on else {
        return bad("speaker must be on/off");
    };
    match dispatch(&s, OpCommand::Speaker(on)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_squelch(
    State(s): State<AppState>,
    Path(v): Path<u16>,
) -> Response {
    if v > 255 {
        return bad("squelch must be 0..=255");
    }
    match dispatch(&s, OpCommand::Squelch(v as u8)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_mod(State(s): State<AppState>, Path(m): Path<String>) -> Response {
    let Some(mode) = parse_mod(&m) else {
        return bad("mod must be am/fm");
    };
    match dispatch(&s, OpCommand::ModTxRx(mode)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_mod_tx_only(State(s): State<AppState>, Path(m): Path<String>) -> Response {
    let Some(mode) = parse_mod(&m) else {
        return bad("mod_tx_only must be am/fm");
    };
    match dispatch(&s, OpCommand::ModTxOnly(mode)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_text(State(s): State<AppState>, Path(t): Path<String>) -> Response {
    let mode = match t.to_lowercase().as_str() {
        "pt" | "voice" | "plain" => TextMode::Pt,
        "ct" | "data" | "cipher" => TextMode::Ct,
        _ => return bad("text must be pt/ct"),
    };
    match dispatch(&s, OpCommand::Text(mode)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_power(State(s): State<AppState>, Path(p): Path<String>) -> Response {
    let pw = match p.to_lowercase().as_str() {
        "lo" | "low" | "0" => PowerLevel::Lo,
        "med" | "medium" | "1" => PowerLevel::Med,
        "hi" | "high" | "2" => PowerLevel::Hi,
        _ => return bad("power must be lo/med/hi"),
    };
    match dispatch(&s, OpCommand::Power(pw)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_scan(State(s): State<AppState>, Path(st): Path<String>) -> Response {
    let on = match parse_on_off(&st) {
        Ok(Some(b)) => b,
        _ => return bad("scan must be on/off"),
    };
    match dispatch(&s, OpCommand::Scan(on)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_scan_list(State(s): State<AppState>, Path(st): Path<String>) -> Response {
    let on = match parse_on_off(&st) {
        Ok(Some(b)) => b,
        _ => return bad("scan_list_member must be on/off"),
    };
    match dispatch(&s, OpCommand::ScanListMember(on)).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

async fn post_store(State(s): State<AppState>) -> Response {
    // `Q` stores the current RAM state to EEPROM. Not a transmit — safe.
    match dispatch(&s, OpCommand::StorePreset).await {
        Ok((c, j)) => (c, j).into_response(),
        Err((c, m)) => (c, m).into_response(),
    }
}

#[derive(Deserialize)]
pub struct TuneRequest {
    pub rx_hz: u32,
    pub tx_hz: u32,
    /// One of: "am", "fm". Sets ModTxRx. Omit to leave modulation untouched.
    pub mode: Option<String>,
    /// One of: "25k", "12.5k", "5k". Defaults to "5k" (finest common grid).
    pub step: Option<String>,
}

#[derive(Serialize)]
pub struct TuneReply {
    pub rx: CommandReply,
    pub tx: CommandReply,
    pub mode: Option<CommandReply>,
}

async fn post_tune(
    State(s): State<AppState>,
    Json(req): Json<TuneRequest>,
) -> Response {
    let step = match req.step.as_deref().unwrap_or("5k") {
        "25k" => Step::Khz25,
        "12.5k" | "12_5k" => Step::Khz12_5,
        "5k" => Step::Khz5,
        other => return bad(format!("unknown step {other:?}; use 25k/12.5k/5k")),
    };
    let band = band_for(req.rx_hz).or_else(|| band_for(req.tx_hz));
    let Some(band) = band else {
        return bad(format!(
            "rx_hz {} / tx_hz {} not in any supported band",
            req.rx_hz, req.tx_hz
        ));
    };
    let rx = match Freq::new(req.rx_hz, band, step) {
        Ok(f) => f,
        Err(e) => return bad(format!("rx_hz invalid: {e}")),
    };
    let tx = match Freq::new(req.tx_hz, band, step) {
        Ok(f) => f,
        Err(e) => return bad(format!("tx_hz invalid: {e}")),
    };

    let rx_reply = match s.radio.send(OpCommand::SetRx(rx)).await {
        Ok(r) => CommandReply::from_response(r).1,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("rx: {e}")).into_response(),
    };
    let tx_reply = match s.radio.send(OpCommand::SetTx(tx)).await {
        Ok(r) => CommandReply::from_response(r).1,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("tx: {e}")).into_response(),
    };
    let mode_reply = if let Some(m) = req.mode.as_deref() {
        let Some(mm) = parse_mod(m) else {
            return bad("mode must be am/fm");
        };
        match s.radio.send(OpCommand::ModTxRx(mm)).await {
            Ok(r) => Some(CommandReply::from_response(r).1),
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("mode: {e}")).into_response(),
        }
    } else {
        None
    };

    Json(TuneReply {
        rx: rx_reply,
        tx: tx_reply,
        mode: mode_reply,
    })
    .into_response()
}

// ---- helpers ----

fn parse_on_off(s: &str) -> Result<Option<bool>, ()> {
    match s.to_lowercase().as_str() {
        "on" | "1" | "true" => Ok(Some(true)),
        "off" | "0" | "false" => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn parse_mod(s: &str) -> Option<ModMode> {
    match s.to_lowercase().as_str() {
        "am" => Some(ModMode::Am),
        "fm" => Some(ModMode::Fm),
        _ => None,
    }
}

fn band_for(hz: u32) -> Option<Band> {
    match hz {
        115_000_000..=173_995_000 => Some(Band::Base),
        225_000_000..=399_995_000 => Some(Band::Base),
        30_000_000..=90_000_000 => Some(Band::Lvhf),
        400_000_000..=420_000_000 => Some(Band::Uhf400),
        _ => None,
    }
}
