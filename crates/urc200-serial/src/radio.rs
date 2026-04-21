//! High-level async dispatcher for the URC-200.
//!
//! A single owner task holds the [`Transport`] and serializes all commands per
//! §4.6.3 ("The RCU cannot send a new command until it receives an ACK/NAK/HT
//! or times out"). Callers obtain a cloneable [`Radio`] handle and await
//! responses via an in-process oneshot channel.
//!
//! The dispatcher:
//!   - encodes `OpCommand`/`Inquiry` and writes them as one atomic byte write
//!   - awaits the response with a configurable timeout
//!   - feeds bytes through `ResponseParser`
//!   - tracks consecutive NAKs (Table 9 rule) and surfaces `RadioError::Fault`
//!     on the third NAK
//!   - exits cleanly when the last `Radio` handle is dropped
//!
//! Out of scope for S-021 (landing later):
//!   - auto Z-resync on timeout (caller policy for now)
//!   - reconnect-on-transport-error
//!   - panic/Drop guarantee that emits `E\r` to unkey the radio (Epic 6)

use crate::{RadioError, Transport, TransportError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};
use urc200_proto::{DispatchOutcome, Inquiry, NakCounter, OpCommand, Response, ResponseParser};

/// Default per-command timeout. At 1200 bps, a 27-byte `?10` response takes
/// ~225 ms to clock out. 500 ms is comfortable; tune down for faster commands.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);
const CHANNEL_CAPACITY: usize = 16;

/// Async handle to a URC-200 over a `Transport`. Clone freely; every clone
/// shares the same serialised dispatcher task.
#[derive(Clone)]
pub struct Radio {
    tx: mpsc::Sender<CommandRequest>,
}

struct CommandRequest {
    bytes: Vec<u8>,
    reply: oneshot::Sender<Result<Response, RadioError>>,
}

impl Radio {
    /// Spawn a dispatcher task around the given transport. The returned handle
    /// can be cloned; the task runs until an explicit shutdown via
    /// [`RadioHandle::shutdown`] or until the host process exits.
    pub fn spawn<T>(transport: T, cmd_timeout: Duration) -> RadioHandle
    where
        T: Transport + 'static,
    {
        let (tx, rx) = mpsc::channel::<CommandRequest>(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(dispatcher_loop(
            Box::new(transport),
            rx,
            shutdown_rx,
            cmd_timeout,
        ));
        RadioHandle {
            radio: Radio { tx },
            shutdown: Some(shutdown_tx),
            task: Some(task),
        }
    }

    /// Send a Table 11 operation command and await the response.
    pub async fn send(&self, cmd: OpCommand) -> Result<Response, RadioError> {
        self.send_raw(cmd.encode()).await
    }

    /// Send a Table 13 status inquiry and await the (data-bearing) response.
    pub async fn query(&self, inq: Inquiry) -> Result<Response, RadioError> {
        self.send_raw(inq.encode().to_vec()).await
    }

    /// Send arbitrary bytes — escape hatch for Table 12 customising commands
    /// or diagnostics not yet covered by a typed enum.
    pub async fn send_raw(&self, bytes: Vec<u8>) -> Result<Response, RadioError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = CommandRequest {
            bytes,
            reply: reply_tx,
        };
        self.tx.send(req).await.map_err(|_| RadioError::Closed)?;
        reply_rx.await.map_err(|_| RadioError::Closed)?
    }
}

/// Owns the dispatcher task. Use [`shutdown`](Self::shutdown) to end the task
/// cleanly — this works even when cloned `Radio` handles are still alive
/// (their subsequent `send`/`query` calls will receive `RadioError::Closed`).
///
/// If dropped without an explicit shutdown, the signal is fired best-effort
/// via [`Drop`] so the task still exits — but the drop cannot `await` the
/// task, so any final I/O in flight isn't drained. Prefer calling
/// [`shutdown().await`](Self::shutdown) from an async context.
pub struct RadioHandle {
    pub radio: Radio,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl RadioHandle {
    /// Signal the dispatcher to exit and await its completion. Any in-flight
    /// command finishes; queued commands return `Closed`.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for RadioHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Can't await the task here; it'll finish on its own now that the
        // shutdown signal has been sent.
    }
}

async fn dispatcher_loop(
    mut transport: Box<dyn Transport>,
    mut cmd_rx: mpsc::Receiver<CommandRequest>,
    mut shutdown: oneshot::Receiver<()>,
    cmd_timeout: Duration,
) {
    let mut parser = ResponseParser::new();
    let mut naks = NakCounter::new();

    loop {
        let req = tokio::select! {
            biased;
            _ = &mut shutdown => return,
            maybe_req = cmd_rx.recv() => match maybe_req {
                Some(r) => r,
                None => return, // all senders dropped — shouldn't normally happen
            },
        };

        // Reset parser state between commands — per §4.6.3 each command/response
        // is a self-contained framing unit.
        parser.reset();

        tracing::debug!(bytes = %display_bytes(&req.bytes), "-> wire");
        if let Err(e) = transport.write_all(&req.bytes).await {
            let _ = req.reply.send(Err(RadioError::Transport(e)));
            continue;
        }

        let outcome = match await_response(&mut transport, &mut parser, cmd_timeout).await {
            Ok(response) => match naks.observe(&response) {
                DispatchOutcome::Fault => Err(RadioError::Fault),
                _ => Ok(response),
            },
            Err(e) => Err(e),
        };

        let _ = req.reply.send(outcome);
    }
}

fn display_bytes(b: &[u8]) -> String {
    let s: String = b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
    format!("{:?} ({} bytes)", s, b.len())
}

async fn await_response(
    transport: &mut Box<dyn Transport>,
    parser: &mut ResponseParser,
    cmd_timeout: Duration,
) -> Result<Response, RadioError> {
    let deadline = Instant::now() + cmd_timeout;
    let mut buf = [0u8; 256];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RadioError::Timeout(cmd_timeout));
        }
        match timeout(remaining, transport.read(&mut buf)).await {
            Err(_) => return Err(RadioError::Timeout(cmd_timeout)),
            Ok(Err(TransportError::Closed)) => return Err(RadioError::Closed),
            Ok(Err(e)) => return Err(RadioError::Transport(e)),
            Ok(Ok(0)) => return Err(RadioError::Closed),
            Ok(Ok(n)) => {
                for &b in &buf[..n] {
                    if let Some(r) = parser.feed(b) {
                        return Ok(r);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockTransport;
    use urc200_proto::{LampLevel, PresetId};

    const ACK: u8 = 0x06;
    const NAK: u8 = 0x15;

    /// Build a mock that answers every write with the given response bytes.
    fn mock_with_responses(responses: &[&[u8]]) -> MockTransport {
        let mut m = MockTransport::new().lax();
        for r in responses {
            m = m.respond(r);
        }
        m
    }

    #[tokio::test]
    async fn send_op_command_ack() {
        let mock = mock_with_responses(&[&[ACK]]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        let r = handle
            .radio
            .send(OpCommand::Lamp(LampLevel::Med))
            .await
            .unwrap();
        assert!(r.is_ack());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn query_returns_data_then_ack() {
        let mock = mock_with_responses(&[b"A1\x06"]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        let r = handle.radio.query(Inquiry::SynthLock).await.unwrap();
        assert!(r.is_ack());
        assert_eq!(r.data(), b"A1");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn timeout_fires_when_radio_silent() {
        let mock = MockTransport::new().lax().silent();
        let handle = Radio::spawn(mock, Duration::from_millis(100));
        let err = handle
            .radio
            .send(OpCommand::Preset(PresetId::new(0).unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(err, RadioError::Timeout(_)));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn three_naks_fault() {
        let mock = mock_with_responses(&[&[NAK], &[NAK], &[NAK]]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        // First two return Ok with a NAK response.
        let r1 = handle.radio.send(OpCommand::Zap).await.unwrap();
        assert!(r1.is_nak());
        let r2 = handle.radio.send(OpCommand::Zap).await.unwrap();
        assert!(r2.is_nak());
        // Third one escalates to Fault.
        let e = handle.radio.send(OpCommand::Zap).await.unwrap_err();
        assert!(matches!(e, RadioError::Fault));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn ack_between_naks_resets_fault_counter() {
        let mock = mock_with_responses(&[&[NAK], &[NAK], &[ACK], &[NAK], &[NAK]]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        assert!(handle.radio.send(OpCommand::Zap).await.unwrap().is_nak());
        assert!(handle.radio.send(OpCommand::Zap).await.unwrap().is_nak());
        assert!(handle.radio.send(OpCommand::Zap).await.unwrap().is_ack());
        // counter was reset — next two NAKs should not fault
        assert!(handle.radio.send(OpCommand::Zap).await.unwrap().is_nak());
        assert!(handle.radio.send(OpCommand::Zap).await.unwrap().is_nak());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn closed_handle_returns_closed_error() {
        let mock = mock_with_responses(&[&[ACK]]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        let radio = handle.radio.clone();
        handle.shutdown().await;
        // Dispatcher task has exited; further sends should fail.
        let err = radio.send(OpCommand::Zap).await.unwrap_err();
        assert!(matches!(err, RadioError::Closed));
    }

    #[tokio::test]
    async fn serializes_concurrent_senders() {
        // Two clones, two commands in flight. The mock hands out responses in
        // the order it receives writes. Both should succeed, neither should
        // mix responses.
        let mock = mock_with_responses(&[b"A1\x06", b"N128\x06"]);
        let handle = Radio::spawn(mock, DEFAULT_TIMEOUT);
        let a = handle.radio.clone();
        let b = handle.radio.clone();
        let (ra, rb) = tokio::join!(
            a.query(Inquiry::SynthLock),
            b.query(Inquiry::Rssi),
        );
        let ra = ra.unwrap();
        let rb = rb.unwrap();
        // Due to mpsc FIFO ordering, a's request arrives first → gets A1;
        // b's request second → gets N128.
        assert_eq!(ra.data(), b"A1");
        assert_eq!(rb.data(), b"N128");
        handle.shutdown().await;
    }
}
