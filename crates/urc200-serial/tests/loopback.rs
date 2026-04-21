//! Hardware loopback integration test.
//!
//! Physical setup required:
//!   - USB-to-RS232 adapter plugged in (PL2303, FTDI, CP2102, etc.).
//!   - DB9 pins 2 (TX) and 3 (RX) shorted to each other — a jumper wire
//!     or a commercial loopback plug. Everything we write comes back byte-for-byte.
//!
//! Run with:
//!   URC_SERIAL_LOOPBACK=/dev/ttyUSB0 cargo test -p urc200-serial --test loopback
//!
//! The test no-ops if the env var is absent, so plain `cargo test` stays green.

use std::time::Duration;
use tokio::time::timeout;
use urc200_serial::{SerialConfig, SerialTransport, Transport};

async fn loopback_path() -> Option<String> {
    std::env::var("URC_SERIAL_LOOPBACK").ok()
}

#[tokio::test]
async fn echoes_simple_ascii() {
    let Some(path) = loopback_path().await else {
        eprintln!("URC_SERIAL_LOOPBACK not set — skipping");
        return;
    };

    let cfg = SerialConfig::urc200(&path);
    let mut t = SerialTransport::open(&cfg).expect("open serial port");

    let msg: &[u8] = b"HELLO URC200";
    t.write_all(msg).await.expect("write");

    // 1200 bps = ~120 bytes/sec → 12 bytes ≈ 100 ms to clock out and back.
    let mut accum = Vec::with_capacity(msg.len());
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while accum.len() < msg.len() && std::time::Instant::now() < deadline {
        let mut buf = [0u8; 32];
        match timeout(Duration::from_millis(500), t.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => accum.extend_from_slice(&buf[..n]),
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(e)) => panic!("read error: {e}"),
        }
    }
    assert_eq!(&accum[..msg.len()], msg, "loopback did not echo exactly");
}

#[tokio::test]
async fn echoes_urc200_style_command_bytes() {
    let Some(path) = loopback_path().await else {
        eprintln!("URC_SERIAL_LOOPBACK not set — skipping");
        return;
    };

    let cfg = SerialConfig::urc200(&path);
    let mut t = SerialTransport::open(&cfg).expect("open serial port");

    // These are the exact wire bytes the radio would receive in normal use.
    let commands: &[&[u8]] = &[
        b"Z",
        b"P3",
        b"L2",
        b"$128",
        b"R251950",
        b"?11",
    ];

    for cmd in commands {
        t.write_all(cmd).await.expect("write");
        let mut got = Vec::with_capacity(cmd.len());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while got.len() < cmd.len() && std::time::Instant::now() < deadline {
            let mut buf = [0u8; 16];
            match timeout(Duration::from_millis(500), t.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => continue,
            }
        }
        assert_eq!(&got[..cmd.len()], *cmd, "echo mismatch for {cmd:?}");
    }
}
