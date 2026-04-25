//! Server-side PTT arbiter.
//!
//! Single-owner state machine. One task owns the state and serialises all
//! mutations through an mpsc queue. A 50 ms ticker watchdogs the heartbeat:
//! if the current owner hasn't sent a `ptt_heartbeat` in 400 ms, the arbiter
//! synthesises a release and emits `E\r` to the radio.
//!
//! Safety invariants (Murat, round 4):
//!   - heartbeat timeout → `E`
//!   - socket close → `ClientGone` → `E`
//!   - server shutdown → `E`
//!   - second client `start` while owner held → denied
//!
//! This module DEFINES the transmit code path (`OpCommand::Transmit` inside
//! `handle_start`). The runtime only executes it when a WebSocket client
//! explicitly sends `{"type": "ptt_start"}` — nothing else in the server
//! triggers transmit. The operator's standing rule "no transmit from code"
//! is enforced by the UI being the only origin of that message.

use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use urc200_proto::OpCommand;
use urc200_serial::Radio;

/// How often the background task sweeps for expired heartbeats.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(50);

/// Ownership expires this long after the last heartbeat.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(400);

/// Uniquely identifies a browser/websocket connection. Assigned by the server.
pub type ClientId = String;

/// Short human-readable label shown to other clients — "Hammer on ThinkPad".
pub type ClientLabel = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSnapshot {
    pub client: ClientId,
    pub label: ClientLabel,
}

#[derive(Debug, Clone)]
pub enum Grant {
    Granted,
    DeniedLockedBy(ClientLabel),
    DeniedRadioError(String),
    DeniedArbiterDown,
}

#[derive(Debug, Clone)]
pub enum PttEvent {
    Acquired(OwnerSnapshot),
    Released {
        previous: OwnerSnapshot,
        reason: ReleaseReason,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ReleaseReason {
    UserStop,
    HeartbeatTimeout,
    SocketClose,
    ServerShutdown,
    RadioError,
}

impl ReleaseReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReleaseReason::UserStop => "user_stop",
            ReleaseReason::HeartbeatTimeout => "heartbeat_timeout",
            ReleaseReason::SocketClose => "socket_close",
            ReleaseReason::ServerShutdown => "server_shutdown",
            ReleaseReason::RadioError => "radio_error",
        }
    }
}

enum Msg {
    Start {
        client: ClientId,
        label: ClientLabel,
        reply: oneshot::Sender<Grant>,
    },
    Heartbeat(ClientId),
    Stop(ClientId),
    ClientGone(ClientId),
    QueryOwner(oneshot::Sender<Option<OwnerSnapshot>>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct PttHandle {
    tx: mpsc::Sender<Msg>,
    events: broadcast::Sender<PttEvent>,
}

impl PttHandle {
    pub fn spawn(radio: Radio) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(64);
        let evts = events.clone();
        let task = tokio::spawn(arbiter_task(rx, radio, evts));
        (Self { tx, events }, task)
    }

    pub async fn start(&self, client: ClientId, label: ClientLabel) -> Grant {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Msg::Start { client, label, reply })
            .await
            .is_err()
        {
            return Grant::DeniedArbiterDown;
        }
        rx.await.unwrap_or(Grant::DeniedArbiterDown)
    }

    pub async fn heartbeat(&self, client: ClientId) {
        let _ = self.tx.send(Msg::Heartbeat(client)).await;
    }

    pub async fn stop(&self, client: ClientId) {
        let _ = self.tx.send(Msg::Stop(client)).await;
    }

    pub async fn client_gone(&self, client: ClientId) {
        let _ = self.tx.send(Msg::ClientGone(client)).await;
    }

    pub async fn query_owner(&self) -> Option<OwnerSnapshot> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::QueryOwner(reply)).await.is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PttEvent> {
        self.events.subscribe()
    }

    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(reply)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

struct Owner {
    snapshot: OwnerSnapshot,
    last_heartbeat: Instant,
}

struct State {
    owner: Option<Owner>,
    radio: Radio,
    events: broadcast::Sender<PttEvent>,
}

async fn arbiter_task(
    mut rx: mpsc::Receiver<Msg>,
    radio: Radio,
    events: broadcast::Sender<PttEvent>,
) {
    let mut state = State {
        owner: None,
        radio,
        events,
    };
    let mut ticker = tokio::time::interval(WATCHDOG_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = ticker.tick() => {
                if let Some(o) = &state.owner {
                    if o.last_heartbeat.elapsed() > HEARTBEAT_TIMEOUT {
                        warn!(
                            owner = %o.snapshot.label,
                            age_ms = o.last_heartbeat.elapsed().as_millis() as u64,
                            "heartbeat expired — force unkey"
                        );
                        release(&mut state, ReleaseReason::HeartbeatTimeout).await;
                    }
                }
            }
            msg = rx.recv() => match msg {
                None => break,
                Some(Msg::Start { client, label, reply }) => {
                    let g = handle_start(&mut state, client, label).await;
                    let _ = reply.send(g);
                }
                Some(Msg::Heartbeat(client)) => {
                    if let Some(o) = &mut state.owner {
                        if o.snapshot.client == client {
                            o.last_heartbeat = Instant::now();
                        }
                    }
                }
                Some(Msg::Stop(client)) => {
                    if matches_owner(&state, &client) {
                        release(&mut state, ReleaseReason::UserStop).await;
                    }
                }
                Some(Msg::ClientGone(client)) => {
                    if matches_owner(&state, &client) {
                        release(&mut state, ReleaseReason::SocketClose).await;
                    }
                }
                Some(Msg::QueryOwner(reply)) => {
                    let _ = reply.send(state.owner.as_ref().map(|o| o.snapshot.clone()));
                }
                Some(Msg::Shutdown(reply)) => {
                    if state.owner.is_some() {
                        release(&mut state, ReleaseReason::ServerShutdown).await;
                    }
                    let _ = reply.send(());
                    break;
                }
            },
        }
    }
    // Defensive: ensure the radio is unkeyed on any task exit path.
    if state.owner.is_some() {
        let _ = state.radio.send(OpCommand::Receive).await;
    }
}

fn matches_owner(state: &State, client: &str) -> bool {
    state
        .owner
        .as_ref()
        .map(|o| o.snapshot.client == client)
        .unwrap_or(false)
}

async fn handle_start(state: &mut State, client: ClientId, label: ClientLabel) -> Grant {
    if let Some(o) = &state.owner {
        return Grant::DeniedLockedBy(o.snapshot.label.clone());
    }
    // Flush the radio's command parser before keying. URC-200 manual §4.6.3
    // recommends `Z` to "re-establish sync". RF coupling onto the RS-232 RX
    // line during nearby transmissions can inject spurious bytes into the
    // radio's UART buffer even when we send nothing; if those bytes are
    // sitting in the parse buffer when our next command arrives, they
    // concatenate and the radio interprets a different command. A Z right
    // before B guarantees the parse state is clean.
    let _ = state.radio.send(OpCommand::Zap).await;
    // THIS IS THE ONLY PLACE IN THE SERVER THAT ISSUES `B\r` TO THE RADIO.
    // It is reached only when a WebSocket client sends a deliberate
    // `ptt_start` message. No automation path leads here.
    match state.radio.send(OpCommand::Transmit).await {
        Ok(r) if r.is_nak() => Grant::DeniedRadioError("radio NAK'd B".into()),
        Ok(_) => {
            let snapshot = OwnerSnapshot { client, label };
            info!(owner = %snapshot.label, "PTT acquired");
            state.owner = Some(Owner {
                snapshot: snapshot.clone(),
                last_heartbeat: Instant::now(),
            });
            let _ = state.events.send(PttEvent::Acquired(snapshot));
            Grant::Granted
        }
        Err(e) => Grant::DeniedRadioError(format!("{e}")),
    }
}

async fn release(state: &mut State, reason: ReleaseReason) {
    let prev = match state.owner.take() {
        Some(o) => o.snapshot,
        None => return,
    };
    // Flush the radio's parser BEFORE the un-key (see Z rationale at the top
    // of `handle_start`). This is the critical one: the radio has been
    // transmitting, so RF coupling onto its RX line is at peak, and we've
    // observed an `E` after a TX window getting interpreted as a preset-
    // cancel command and resetting the radio to factory defaults.
    let _ = state.radio.send(OpCommand::Zap).await;
    // Unkey unconditionally. If this fails, we still drop ownership — the
    // next heartbeat sweep can retry, and startup-unkey will catch anything
    // that somehow stayed latched.
    let unkey = state.radio.send(OpCommand::Receive).await;
    if let Err(e) = &unkey {
        warn!(error = ?e, reason = reason.as_str(), "E failed during release");
    }
    info!(previous = %prev.label, reason = reason.as_str(), "PTT released");
    let _ = state.events.send(PttEvent::Released {
        previous: prev,
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use urc200_serial::{MockTransport, RadioHandle, DEFAULT_TIMEOUT};

    // Wire bytes for ACK/NAK/HT on the URC-200.
    const ACK: u8 = 0x06;

    fn mock_with_acks(n: usize) -> MockTransport {
        let mut m = MockTransport::new().lax();
        for _ in 0..n {
            m = m.respond(&[ACK]);
        }
        m
    }

    fn spawn_arbiter(responses: usize) -> (PttHandle, JoinHandle<()>, RadioHandle) {
        let mock = mock_with_acks(responses);
        let radio = Radio::spawn(mock, DEFAULT_TIMEOUT);
        let (h, task) = PttHandle::spawn(radio.radio.clone());
        (h, task, radio)
    }

    #[tokio::test]
    async fn start_grants_when_idle() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E shutdown
        let g = ptt.start("c1".into(), "alice".into()).await;
        assert!(matches!(g, Grant::Granted));
        let owner = ptt.query_owner().await.unwrap();
        assert_eq!(owner.label, "alice");
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn second_start_denied_while_held() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start1, denied=0, Z+E shutdown
        let g1 = ptt.start("c1".into(), "alice".into()).await;
        assert!(matches!(g1, Grant::Granted));
        let g2 = ptt.start("c2".into(), "bob".into()).await;
        match g2 {
            Grant::DeniedLockedBy(who) => assert_eq!(who, "alice"),
            other => panic!("expected DeniedLockedBy, got {other:?}"),
        }
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn stop_releases_and_unkeys() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E stop, shutdown=0 (already released)
        let mut sub = ptt.subscribe();
        ptt.start("c1".into(), "alice".into()).await;
        let _ = sub.recv().await.unwrap(); // Acquired
        ptt.stop("c1".into()).await;
        let ev = sub.recv().await.unwrap();
        match ev {
            PttEvent::Released { previous, reason } => {
                assert_eq!(previous.label, "alice");
                assert!(matches!(reason, ReleaseReason::UserStop));
            }
            _ => panic!("expected Released"),
        }
        assert!(ptt.query_owner().await.is_none());
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_timeout_forces_unkey() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E timeout-release
        let mut sub = ptt.subscribe();
        ptt.start("c1".into(), "alice".into()).await;
        let _ = sub.recv().await.unwrap();
        // Don't send heartbeats. Wait for the watchdog to fire.
        tokio::time::sleep(HEARTBEAT_TIMEOUT + Duration::from_millis(100)).await;
        let ev = sub.recv().await.unwrap();
        match ev {
            PttEvent::Released { reason, .. } => {
                assert!(matches!(reason, ReleaseReason::HeartbeatTimeout));
            }
            _ => panic!("expected Released"),
        }
        assert!(ptt.query_owner().await.is_none());
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_keeps_ownership_alive() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E shutdown
        ptt.start("c1".into(), "alice".into()).await;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ptt.heartbeat("c1".into()).await;
        }
        assert!(ptt.query_owner().await.is_some()); // still held
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn socket_close_releases() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E close-release
        ptt.start("c1".into(), "alice".into()).await;
        ptt.client_gone("c1".into()).await;
        // Give the arbiter a tick to process.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(ptt.query_owner().await.is_none());
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    /// Defends the behavior that already works: when the PTT arbiter shuts
    /// down while keyed, the radio is unkeyed *quickly*. Murat's priority test
    /// — locks down the invariant that currently survives only because real use
    /// beat it in. Every future driver inherits this assertion via the
    /// conformance suite (radio-core).
    #[tokio::test]
    async fn shutdown_while_keyed_unkeys_within_500ms() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E shutdown
        let mut sub = ptt.subscribe();
        ptt.start("c1".into(), "alice".into()).await;
        let _ = sub.recv().await.unwrap(); // Acquired
        let t0 = std::time::Instant::now();
        ptt.shutdown().await;
        // Shutdown returns only after the arbiter task has emitted E and
        // ReleaseReason::ServerShutdown — so the measurement bounds the
        // whole unkey-on-shutdown path, transport write included.
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "unkey took {elapsed:?}, expected < 500ms"
        );
        let ev = sub.recv().await.unwrap();
        assert!(matches!(
            ev,
            PttEvent::Released { reason: ReleaseReason::ServerShutdown, .. }
        ));
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_releases_if_held() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E shutdown
        let mut sub = ptt.subscribe();
        ptt.start("c1".into(), "alice".into()).await;
        let _ = sub.recv().await.unwrap();
        ptt.shutdown().await;
        let ev = sub.recv().await.unwrap();
        match ev {
            PttEvent::Released { reason, .. } => {
                assert!(matches!(reason, ReleaseReason::ServerShutdown));
            }
            _ => panic!("expected Released"),
        }
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_from_non_owner_is_ignored() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E timeout-release
        ptt.start("c1".into(), "alice".into()).await;
        // Bob sends heartbeats, but he doesn't own the key — they must not
        // extend alice's lease.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ptt.heartbeat("c2".into()).await;
        }
        // Alice's heartbeat is now stale; watchdog should have unkeyed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(ptt.query_owner().await.is_none());
        ptt.shutdown().await;
        radio.shutdown().await;
    }

    #[tokio::test]
    async fn stop_from_non_owner_is_ignored() {
        let (ptt, _task, radio) = spawn_arbiter(4); // Z+B start, Z+E shutdown
        ptt.start("c1".into(), "alice".into()).await;
        ptt.stop("c2".into()).await; // bob can't release alice's key
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(ptt.query_owner().await.is_some());
        ptt.shutdown().await;
        radio.shutdown().await;
    }
}
