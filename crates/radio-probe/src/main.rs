//! Diagnostic tool — send a single command to the URC-200 and hex-dump whatever
//! comes back.
//!
//! Usage:
//!   cargo run -p radio-probe                    # sends "?01" (synth lock)
//!   cargo run -p radio-probe -- '?11'           # sends general status
//!   cargo run -p radio-probe -- 'Z'             # resync
//!   URC_PORT=/dev/ttyUSB1 cargo run -p radio-probe
//!
//! Safe commands to experiment with: ?01, ?03, ?08, ?10, ?11, ?12, ?13, Z, +, L0..L3, J0/J1.
//! DO NOT send B, *1, I, K, or any ^/W/! commands without understanding what they do.

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::time::{timeout, Instant};
use urc200_proto::{
    GeneralStatus, Mode, PresetSnapshot, Response, ResponseParser, Rssi, SquelchStatus, SynthLock,
};
use urc200_serial::{SerialConfig, SerialTransport, Transport};

#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::var("URC_PORT").unwrap_or_else(|_| "/dev/ttyUSB0".to_string());
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "?01".to_string());
    let wait_secs: u64 = std::env::var("URC_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    // Hard-refuse commands that transmit, beacon, wipe presets, or self-calibrate.
    // No override flag — user explicit standing rule.
    refuse_dangerous_command(&cmd)?;

    println!("Port:    {port}");
    println!("Command: {cmd:?} ({} bytes: {})", cmd.len(), hex_ascii(cmd.as_bytes()));
    println!("Waiting {wait_secs} s for response...\n");

    let cfg = SerialConfig::urc200(&port);
    let mut t = SerialTransport::open(&cfg).with_context(|| format!("open {port}"))?;

    t.write_all(cmd.as_bytes()).await.context("write command")?;
    let tx_at = Instant::now();
    println!("-> TX at t=0");

    let mut parser = ResponseParser::new();
    let mut raw = Vec::<u8>::with_capacity(128);
    let deadline = tx_at + Duration::from_secs(wait_secs);

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut buf = [0u8; 64];
        match timeout(remaining, t.read(&mut buf)).await {
            Err(_) => break, // overall deadline hit
            Ok(Err(e)) => {
                eprintln!("read error: {e:?}");
                break;
            }
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let elapsed_ms = tx_at.elapsed().as_millis();
                println!(
                    "<- RX  t={elapsed_ms:>4} ms  +{n} bytes:  {}",
                    hex_ascii(&buf[..n])
                );
                for &b in &buf[..n] {
                    raw.push(b);
                    if let Some(r) = parser.feed(b) {
                        println!("   ** Response: {}", describe_response(&r));
                        if let Some(decoded) = try_typed_decode(cmd.as_bytes(), r.data()) {
                            println!("   ** Decoded : {decoded}");
                        }
                    }
                }
            }
        }
    }

    println!("\n========== SUMMARY ==========");
    println!("Total bytes received: {}", raw.len());
    if raw.is_empty() {
        println!("  (nothing — check cable, radio power, red/white orientation, port path)");
    } else {
        println!("  hex:   {}", raw.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
        println!("  ascii: {:?}", String::from_utf8_lossy(&raw));
    }
    if parser.overflowed() {
        println!("WARNING: parser overflow — stream did not contain an ACK/NAK/HT within 256 bytes.");
    }
    Ok(())
}

fn refuse_dangerous_command(cmd: &str) -> Result<()> {
    let first = cmd.chars().next().unwrap_or(' ');
    match first {
        'B' => anyhow::bail!("REFUSED: 'B' keys the transmitter. This tool never transmits."),
        '*' => {
            if cmd.starts_with("*1") {
                anyhow::bail!("REFUSED: '*1' enables beacon mode (continuous carrier). This tool never transmits.");
            }
        }
        'I' => anyhow::bail!("REFUSED: 'I' wipes all presets to defaults. Use the radio's keypad if you really want this."),
        'K' => anyhow::bail!("REFUSED: 'K' self-calibration requires a 50Ω load and can damage gear if the load is absent."),
        _ => {}
    }
    Ok(())
}

fn hex_ascii(bytes: &[u8]) -> String {
    let hex = bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let ascii: String = bytes
        .iter()
        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
        .collect();
    format!("[{hex}] {ascii:?}")
}

fn try_typed_decode(sent_cmd: &[u8], data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    match sent_cmd {
        b"?01" => SynthLock::from_bytes(data).map(|x| {
            format!("SynthLock: locked={} ({})", x.0, if x.0 { "LOCKED" } else { "UNLOCKED" })
        }),
        b"?03" => Rssi::from_bytes(data).map(|x| format!("Rssi: {}/255", x.0)),
        b"?10" => PresetSnapshot::from_bytes(data).map(|s| {
            format!(
                "PresetSnapshot: preset=P{}, tx={:.4} MHz, rx={:.4} MHz, \
                 mod_tx_rx={:?}, mod_tx_only={:?}, power={:?}, scan_list={}",
                s.preset.get(),
                s.tx_hz as f64 / 1_000_000.0,
                s.rx_hz as f64 / 1_000_000.0,
                s.mod_tx_rx,
                s.mod_tx_only,
                s.power,
                s.on_scan_list,
            )
        }),
        b"?11" => GeneralStatus::from_bytes(data).map(|g| {
            format!(
                "GeneralStatus: text={:?}, speaker={}, lamp={:?}, options_byte={} \
                 (ebn30={}, ebn400={}), overtemp={}",
                g.text_mode,
                g.speaker_on,
                g.lamp,
                g.options_byte,
                g.has_ebn30(),
                g.has_ebn400(),
                g.overtemp,
            )
        }),
        b"?12" => Mode::from_bytes(data).map(|m| format!("Mode: {m:?}")),
        b"?13" => SquelchStatus::from_bytes(data).map(|s| format!("SquelchStatus: {s:?}")),
        _ => None,
    }
}

fn describe_response(r: &Response) -> String {
    match r {
        Response::Ack { data } if data.is_empty() => "ACK (bare, 0x06)".into(),
        Response::Ack { data } => format!(
            "ACK (0x06) with data {:?} -> ascii {:?}",
            data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(data)
        ),
        Response::Nak { data } if data.is_empty() => "NAK (bare, 0x15)".into(),
        Response::Nak { data } => format!(
            "NAK (0x15) with preceding bytes {:?}",
            String::from_utf8_lossy(data)
        ),
        Response::Ht { data } if data.is_empty() => {
            "HT (bare, 0x09 — first remote command after keypad mode)".into()
        }
        Response::Ht { data } => format!(
            "HT (0x09) with data {:?} -> ascii {:?} — first remote command after keypad mode",
            data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(data)
        ),
    }
}
