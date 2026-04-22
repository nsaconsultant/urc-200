//! HTTP routes for the channel library + CSV import.
//!
//! Routes (all under `/api/channels`):
//!   - `GET  /`                      — list all channels (optional `?group=X`)
//!   - `GET  /groups`                — list groups + counts
//!   - `POST /import?group=Name`     — import CSV text (body = raw CSV), auto-detects schema
//!   - `POST /:id/tune`              — apply the channel to the radio (SetRx + SetTx + optional Mod)
//!   - `DELETE /:id`                 — delete one channel
//!   - `DELETE /groups/:name`        — delete an entire group

use crate::db::{Channel, Db, GroupSummary, OwnedNewChannel};
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};
use urc200_proto::{Band, Freq, ModMode, OpCommand, Step};
use urc200_serial::RadioError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/groups", get(list_groups))
        .route("/import", post(import))
        .route("/:id/tune", post(tune_channel))
        .route("/:id", delete(delete_channel))
        .route("/groups/:name", delete(delete_group))
}

// ===== list / groups =====

#[derive(Deserialize)]
struct ListQuery {
    group: Option<String>,
}

async fn list(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Response {
    match s.db.list_channels(q.group).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    }
}

async fn list_groups(State(s): State<AppState>) -> Response {
    match s.db.list_groups().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    }
}

// ===== import =====

#[derive(Deserialize)]
struct ImportQuery {
    group: String,
}

#[derive(Serialize)]
struct ImportReply {
    imported: usize,
    skipped: Vec<String>,
    group: String,
    profile: &'static str,
}

async fn import(
    State(s): State<AppState>,
    Query(q): Query<ImportQuery>,
    body: String,
) -> Response {
    if q.group.is_empty() {
        return (StatusCode::BAD_REQUEST, "group query param required").into_response();
    }
    let parsed = match parse_csv(&body, &q.group) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("csv: {e}")).into_response(),
    };
    let profile = parsed.profile;
    let skipped = parsed.skipped;
    // Re-import semantics: importing into "Aviation" replaces Aviation's rows.
    // Avoids duplicate rows on repeat imports of the same CSV.
    let n = match s
        .db
        .insert_many(parsed.channels, Some(q.group.clone()))
        .await
    {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    };
    info!(group = %q.group, imported = n, skipped = skipped.len(), profile, "csv import");
    s.emit_state(serde_json::json!({ "type": "channels_changed" }));
    Json(ImportReply {
        imported: n,
        skipped,
        group: q.group,
        profile,
    })
    .into_response()
}

// ===== delete =====

async fn delete_channel(State(s): State<AppState>, Path(id): Path<i64>) -> Response {
    match s.db.delete_channel(id).await {
        Ok(true) => {
            s.emit_state(serde_json::json!({ "type": "channels_changed" }));
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such channel").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    }
}

async fn delete_group(State(s): State<AppState>, Path(name): Path<String>) -> Response {
    match s.db.delete_group(name).await {
        Ok(n) => {
            s.emit_state(serde_json::json!({ "type": "channels_changed" }));
            Json(serde_json::json!({ "deleted": n })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    }
}

// ===== tune =====

#[derive(Serialize)]
struct TuneReply {
    channel: Channel,
    rx: CommandStatus,
    tx: CommandStatus,
    mode: Option<CommandStatus>,
}

#[derive(Serialize)]
struct CommandStatus {
    ok: bool,
    response_kind: &'static str,
}

async fn tune_channel(State(s): State<AppState>, Path(id): Path<i64>) -> Response {
    let ch = match s.db.get_channel(id).await {
        Ok(Some(c)) => c,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such channel").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    };

    // Find the best band that contains both frequencies.
    let band = band_for(ch.rx_hz).or_else(|| band_for(ch.tx_hz));
    let Some(band) = band else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "channel {} freqs ({} / {}) not in any URC-200 band",
                ch.name, ch.rx_hz, ch.tx_hz
            ),
        )
            .into_response();
    };
    let step = ch
        .step_khz
        .and_then(|k| match k {
            k if (k - 25.0).abs() < 0.01 => Some(Step::Khz25),
            k if (k - 12.5).abs() < 0.01 => Some(Step::Khz12_5),
            k if (k - 5.0).abs() < 0.01 => Some(Step::Khz5),
            _ => None,
        })
        .unwrap_or(Step::Khz5);

    let rx = match Freq::new(ch.rx_hz, band, step) {
        Ok(f) => f,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("rx: {e}")).into_response(),
    };
    let tx = match Freq::new(ch.tx_hz, band, step) {
        Ok(f) => f,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("tx: {e}")).into_response(),
    };

    let rx_status = send_and_tag(&s, OpCommand::SetRx(rx)).await;
    if let Err(e) = rx_status.as_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("rx: {e}")).into_response();
    }
    let tx_status = send_and_tag(&s, OpCommand::SetTx(tx)).await;
    if let Err(e) = tx_status.as_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("tx: {e}")).into_response();
    }
    let mode_status = if let Some(m) = ch.mode.as_deref() {
        let Some(mm) = parse_mode(m) else {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown mode {:?}", m),
            )
                .into_response();
        };
        let r = send_and_tag(&s, OpCommand::ModTxRx(mm)).await;
        if let Err(e) = r.as_err() {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("mode: {e}")).into_response();
        }
        Some(r.status)
    } else {
        None
    };

    // Auto-apply CTCSS from the channel definition (or clear it if the channel
    // has no tone). Mirrors what an inline hardware CTCSS encoder would do.
    s.audio_tx.ctcss.set(ch.ctcss_hz);
    // Announce the CTCSS change so any other browser's UI reflects it.
    s.emit_state(s.ctcss_snapshot());

    Json(TuneReply {
        channel: ch,
        rx: rx_status.status,
        tx: tx_status.status,
        mode: mode_status,
    })
    .into_response()
}

struct SendResult {
    status: CommandStatus,
    err: Option<String>,
}

impl SendResult {
    fn as_err(&self) -> std::result::Result<(), String> {
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

async fn send_and_tag(s: &AppState, cmd: OpCommand) -> SendResult {
    match s.radio.send(cmd).await {
        Ok(r) if r.is_nak() => SendResult {
            status: CommandStatus {
                ok: false,
                response_kind: "nak",
            },
            err: None, // NAK is a soft failure; let the caller decide
        },
        Ok(_) => SendResult {
            status: CommandStatus {
                ok: true,
                response_kind: "ack",
            },
            err: None,
        },
        Err(RadioError::Timeout(d)) => SendResult {
            status: CommandStatus {
                ok: false,
                response_kind: "timeout",
            },
            err: Some(format!("timeout after {d:?}")),
        },
        Err(e) => SendResult {
            status: CommandStatus {
                ok: false,
                response_kind: "error",
            },
            err: Some(format!("{e}")),
        },
    }
}

// ===== CSV parsing =====

struct ParsedCsv {
    channels: Vec<OwnedNewChannel>,
    skipped: Vec<String>,
    profile: &'static str,
}

/// Auto-detect the schema by header names. Known profiles:
///   - "satcom":    `Downlink`, `Uplink`, `Name`              (FLTSAT-style)
///   - "ham_full":  `Receive Frequency`, `Transmit Frequency`, `Name`, optional `Operating Mode`, `Step`
///   - "canonical": `name`, `rx_mhz`, `tx_mhz`, `mode`(opt), `step_khz`(opt), `notes`(opt)
fn parse_csv(body: &str, group: &str) -> Result<ParsedCsv, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());

    let headers = rdr.headers().map_err(|e| e.to_string())?.clone();
    let hmap: HashMap<String, usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| (h.to_lowercase(), i))
        .collect();

    let has = |keys: &[&str]| -> Option<usize> {
        for k in keys {
            if let Some(&i) = hmap.get(&k.to_lowercase()) {
                return Some(i);
            }
        }
        None
    };

    let (profile, rx_i, tx_i, name_i, mode_i, step_i, notes_i, ctcss_i): (
        &'static str,
        usize,
        usize,
        usize,
        Option<usize>,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    );

    if let (Some(dl), Some(ul), Some(nm)) = (
        has(&["Downlink"]),
        has(&["Uplink"]),
        has(&["Name"]),
    ) {
        profile = "satcom";
        rx_i = dl;
        tx_i = ul;
        name_i = nm;
        mode_i = None;
        step_i = has(&["Bandwidth KHz", "Step"]);
        notes_i = has(&["Comment", "Notes"]);
        ctcss_i = has(&["CTCSS", "Tone"]);
    } else if let (Some(rx), Some(tx), Some(nm)) = (
        has(&["Receive Frequency", "RX Frequency"]),
        has(&["Transmit Frequency", "TX Frequency"]),
        has(&["Name"]),
    ) {
        profile = "ham_full";
        rx_i = rx;
        tx_i = tx;
        name_i = nm;
        mode_i = has(&["Operating Mode", "Mode"]);
        step_i = has(&["Step"]);
        notes_i = has(&["Comment", "Notes"]);
        ctcss_i = has(&["CTCSS", "Tone"]);
    } else if let (Some(rx), Some(tx), Some(nm)) = (
        has(&["rx_mhz", "rx"]),
        has(&["tx_mhz", "tx"]),
        has(&["name"]),
    ) {
        profile = "canonical";
        rx_i = rx;
        tx_i = tx;
        name_i = nm;
        mode_i = has(&["mode"]);
        step_i = has(&["step_khz", "step"]);
        notes_i = has(&["notes", "comment"]);
        ctcss_i = has(&["ctcss_hz", "ctcss", "tone"]);
    } else {
        return Err(format!(
            "could not auto-detect CSV profile; headers were: {:?}. \
             Supported: satcom (Downlink/Uplink/Name), ham_full (Receive Frequency/Transmit Frequency/Name), \
             canonical (name/rx_mhz/tx_mhz)",
            headers.iter().collect::<Vec<_>>()
        ));
    };

    let mut channels = Vec::new();
    let mut skipped = Vec::new();

    for (row_num, rec) in rdr.records().enumerate() {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                skipped.push(format!("row {}: {e}", row_num + 2));
                continue;
            }
        };

        let name = rec.get(name_i).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let rx_raw = rec.get(rx_i).unwrap_or("").trim();
        let tx_raw = rec.get(tx_i).unwrap_or("").trim();
        let rx_hz = match parse_mhz_to_hz(rx_raw) {
            Some(v) => v,
            None => {
                skipped.push(format!("{name}: rx {rx_raw:?} not a MHz value"));
                continue;
            }
        };
        let tx_hz = match parse_mhz_to_hz(tx_raw) {
            Some(v) => v,
            None => {
                skipped.push(format!("{name}: tx {tx_raw:?} not a MHz value"));
                continue;
            }
        };
        // Don't skip out-of-band rows — the library is a station-wide reference,
        // not a URC-200-specific tuning target. Tune-time validation will still
        // reject frequencies the radio can't produce.
        let mode = mode_i
            .and_then(|i| rec.get(i))
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .and_then(|s| normalize_mode(&s));
        let step_khz = step_i
            .and_then(|i| rec.get(i))
            .and_then(parse_step_khz);
        let notes = notes_i
            .and_then(|i| rec.get(i))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let ctcss_hz = ctcss_i
            .and_then(|i| rec.get(i))
            .and_then(parse_ctcss);

        channels.push(OwnedNewChannel {
            group_name: group.to_string(),
            name: name.to_string(),
            rx_hz,
            tx_hz,
            mode,
            step_khz,
            notes,
            ctcss_hz,
        });
    }

    if channels.is_empty() && skipped.is_empty() {
        warn!(profile, "csv import produced 0 channels and 0 skips — empty file?");
    }
    Ok(ParsedCsv {
        channels,
        skipped,
        profile,
    })
}

fn parse_mhz_to_hz(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let cleaned = s.trim().replace(',', "");
    // Values like "SHF" or "DIG" should be rejected.
    let f: f64 = cleaned.parse().ok()?;
    if !(0.0..=10_000.0).contains(&f) {
        return None;
    }
    Some((f * 1_000_000.0).round() as u32)
}

fn parse_step_khz(s: &str) -> Option<f32> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    // Accept "5 kHz", "12.5 kHz", "25", etc.
    let numeric: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    numeric.parse().ok()
}

/// Parse CTCSS values like `"162.2 Hz"`, `"91.5"`, `"None"`. Returns None for
/// empty, "None", or anything that doesn't start with a positive number.
fn parse_ctcss(s: &str) -> Option<f32> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        return None;
    }
    let numeric: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v: f32 = numeric.parse().ok()?;
    if (30.0..300.0).contains(&v) {
        Some(v)
    } else {
        None
    }
}

fn normalize_mode(s: &str) -> Option<String> {
    let s = s.to_lowercase();
    if s.contains("fm") {
        Some("fm".into())
    } else if s.contains("am") {
        Some("am".into())
    } else {
        None
    }
}

fn parse_mode(s: &str) -> Option<ModMode> {
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

// Silence unused import warning when no path currently uses GroupSummary.
#[allow(dead_code)]
fn _unused(_g: GroupSummary) {}
