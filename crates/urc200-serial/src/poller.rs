//! Background telemetry poller.
//!
//! Spawns one task that issues Table 13 inquiries at per-inquiry cadences and
//! broadcasts typed updates to any number of UI/logger subscribers. The
//! underlying [`Radio`] dispatcher serialises commands per §4.6.3, so the
//! poller naturally yields to user commands — if a user command arrives while
//! a poll is in flight, it waits for the in-flight response (≤ ~100 ms) and
//! then runs before the next scheduled poll.
//!
//! Default cadences (tuned for a live Operate-Mode dashboard):
//!
//! | Inquiry          | Period |
//! |------------------|--------|
//! | `?03` RSSI       | 500 ms |
//! | `?13` Squelch    | 500 ms |
//! | `?11` General    | 1 s    |
//! | `?12` Mode       | 1 s    |
//! | `?10` Preset     | 2 s    |
//! | `?01` SynthLock  | 5 s    |
//!
//! Set a cadence to `None` to disable that inquiry. Set the backoff mode to
//! [`BackoffMode::Idle`] to multiply every period by 10 (e.g. when the UI
//! window is hidden or the tab loses focus).

use crate::{Radio, RadioError};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use urc200_proto::{
    GeneralStatus, Inquiry, Mode, PresetSnapshot, Response, Rssi, SquelchStatus, SynthLock,
};

const BROADCAST_CAPACITY: usize = 64;
const CONTROL_CAPACITY: usize = 16;

/// A single typed telemetry update from the radio.
#[derive(Debug, Clone)]
pub enum TelemetryUpdate {
    Rssi(Rssi),
    Squelch(SquelchStatus),
    General(GeneralStatus),
    Mode(Mode),
    Preset(PresetSnapshot),
    SynthLock(SynthLock),
    /// A scheduled inquiry failed. Poller keeps running.
    Error {
        inquiry: Inquiry,
        kind: PollErrorKind,
    },
}

#[derive(Debug, Clone)]
pub enum PollErrorKind {
    /// Dispatcher returned `RadioError::Timeout`.
    Timeout,
    /// Radio returned NAK or decoder rejected the data.
    Decode { raw: Vec<u8> },
    /// Transport-level failure (cable pulled, port closed, etc.).
    Transport(String),
    /// Three consecutive NAKs — fault. The poller keeps trying.
    Fault,
}

/// Pause / throttle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffMode {
    /// Normal cadence — user is actively watching.
    Normal,
    /// Multiply every period by 10 — UI is hidden / backgrounded.
    Idle,
    /// Stop polling entirely. Still accepts control messages.
    Paused,
}

enum Control {
    SetCadence(Inquiry, Option<Duration>),
    SetBackoff(BackoffMode),
}

/// Handle to the poller task. Clone-free — only the spawner owns it. Drop to
/// shut down; call [`shutdown`](Self::shutdown) to await clean exit.
pub struct Poller {
    events: broadcast::Sender<TelemetryUpdate>,
    control: mpsc::Sender<Control>,
    task: JoinHandle<()>,
}

impl Poller {
    /// Spawn the poller with the default schedule.
    pub fn spawn(radio: Radio) -> Self {
        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(CONTROL_CAPACITY);
        let schedules = default_schedules();
        let task = tokio::spawn(poll_loop(radio, ctrl_rx, events_tx.clone(), schedules));
        Self {
            events: events_tx,
            control: ctrl_tx,
            task,
        }
    }

    /// Subscribe to the broadcast stream. Late subscribers miss earlier events;
    /// slow subscribers may lag (tokio `broadcast` drops the oldest).
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryUpdate> {
        self.events.subscribe()
    }

    /// Clone the broadcast sender. Useful when shared application state needs
    /// to hand out subscribers without owning the `Poller` itself.
    pub fn sender(&self) -> broadcast::Sender<TelemetryUpdate> {
        self.events.clone()
    }

    /// Change (or disable with `None`) the cadence for a specific inquiry.
    pub async fn set_cadence(&self, inquiry: Inquiry, period: Option<Duration>) {
        let _ = self.control.send(Control::SetCadence(inquiry, period)).await;
    }

    /// Switch between normal, idle, and paused polling.
    pub async fn set_backoff(&self, mode: BackoffMode) {
        let _ = self.control.send(Control::SetBackoff(mode)).await;
    }

    /// Drop the control handle and wait for the task to finish. The poller
    /// stops on its next control-channel check or scheduled poll, whichever
    /// comes first.
    pub async fn shutdown(self) {
        drop(self.control);
        let _ = self.task.await;
    }
}

fn default_schedules() -> HashMap<Inquiry, Duration> {
    use Inquiry::*;
    [
        (Rssi, Duration::from_millis(500)),
        (SquelchStatus, Duration::from_millis(500)),
        (GeneralStatus, Duration::from_secs(1)),
        (Mode, Duration::from_secs(1)),
        (PresetSnapshot, Duration::from_secs(2)),
        (SynthLock, Duration::from_secs(5)),
    ]
    .into_iter()
    .collect()
}

async fn poll_loop(
    radio: Radio,
    mut control: mpsc::Receiver<Control>,
    events: broadcast::Sender<TelemetryUpdate>,
    mut schedules: HashMap<Inquiry, Duration>,
) {
    let mut last_fired: HashMap<Inquiry, Instant> = HashMap::new();
    // Seed last_fired so every inquiry runs roughly now on startup.
    let start = Instant::now();
    for k in schedules.keys() {
        last_fired.insert(*k, start);
    }

    let mut backoff = BackoffMode::Normal;

    loop {
        // When Paused, just wait on control.
        if backoff == BackoffMode::Paused {
            match control.recv().await {
                None => return,
                Some(c) => apply_control(&mut schedules, &mut backoff, &mut last_fired, c),
            }
            continue;
        }

        let (next_due, next_inq) = match next_due_inquiry(&schedules, &last_fired, backoff) {
            Some(pair) => pair,
            None => {
                // No schedules left; block on control messages.
                match control.recv().await {
                    None => return,
                    Some(c) => apply_control(&mut schedules, &mut backoff, &mut last_fired, c),
                }
                continue;
            }
        };

        tokio::select! {
            biased;
            ctrl = control.recv() => {
                match ctrl {
                    None => return,
                    Some(c) => apply_control(&mut schedules, &mut backoff, &mut last_fired, c),
                }
            }
            _ = sleep_until(next_due) => {
                last_fired.insert(next_inq, Instant::now());
                let update = run_inquiry(&radio, next_inq).await;
                // send returns Err if no receivers; that's fine, we drop.
                let _ = events.send(update);
            }
        }
    }
}

fn next_due_inquiry(
    schedules: &HashMap<Inquiry, Duration>,
    last_fired: &HashMap<Inquiry, Instant>,
    backoff: BackoffMode,
) -> Option<(Instant, Inquiry)> {
    let mult: u32 = match backoff {
        BackoffMode::Normal => 1,
        BackoffMode::Idle => 10,
        BackoffMode::Paused => return None,
    };
    schedules
        .iter()
        .map(|(inq, period)| {
            let effective = *period * mult;
            let last = last_fired.get(inq).copied().unwrap_or_else(Instant::now);
            (last + effective, *inq)
        })
        .min_by_key(|(due, _)| *due)
}

fn apply_control(
    schedules: &mut HashMap<Inquiry, Duration>,
    backoff: &mut BackoffMode,
    last_fired: &mut HashMap<Inquiry, Instant>,
    c: Control,
) {
    match c {
        Control::SetCadence(inq, Some(period)) => {
            schedules.insert(inq, period);
            last_fired.entry(inq).or_insert_with(Instant::now);
        }
        Control::SetCadence(inq, None) => {
            schedules.remove(&inq);
            last_fired.remove(&inq);
        }
        Control::SetBackoff(m) => {
            *backoff = m;
        }
    }
}

async fn run_inquiry(radio: &Radio, inq: Inquiry) -> TelemetryUpdate {
    match radio.query(inq).await {
        Ok(resp) => decode_telemetry(inq, &resp).unwrap_or_else(|| TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Decode {
                raw: resp.data().to_vec(),
            },
        }),
        Err(RadioError::Timeout(_)) => TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Timeout,
        },
        Err(RadioError::Fault) => TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Fault,
        },
        Err(RadioError::Transport(e)) => TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Transport(e.to_string()),
        },
        Err(RadioError::Closed) => TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Transport("radio handle closed".into()),
        },
    }
}

fn decode_telemetry(inq: Inquiry, resp: &Response) -> Option<TelemetryUpdate> {
    // A NAK response becomes a Decode error; an ACK/HT with data gets decoded.
    if resp.is_nak() {
        return Some(TelemetryUpdate::Error {
            inquiry: inq,
            kind: PollErrorKind::Decode {
                raw: resp.data().to_vec(),
            },
        });
    }
    let data = resp.data();
    match inq {
        Inquiry::Rssi => Rssi::from_bytes(data).map(TelemetryUpdate::Rssi),
        Inquiry::SquelchStatus => SquelchStatus::from_bytes(data).map(TelemetryUpdate::Squelch),
        Inquiry::GeneralStatus => GeneralStatus::from_bytes(data).map(TelemetryUpdate::General),
        Inquiry::Mode => Mode::from_bytes(data).map(TelemetryUpdate::Mode),
        Inquiry::PresetSnapshot => PresetSnapshot::from_bytes(data).map(TelemetryUpdate::Preset),
        Inquiry::SynthLock => SynthLock::from_bytes(data).map(TelemetryUpdate::SynthLock),
        // Unsupported typed decode for this inquiry — surface as Decode error.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockTransport;

    #[test]
    fn decode_rssi_ack() {
        let resp = Response::Ack {
            data: b"N128".to_vec(),
        };
        let update = decode_telemetry(Inquiry::Rssi, &resp).unwrap();
        match update {
            TelemetryUpdate::Rssi(r) => assert_eq!(r.0, 128),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_nak_becomes_decode_error() {
        let resp = Response::Nak { data: vec![] };
        let update = decode_telemetry(Inquiry::Rssi, &resp).unwrap();
        assert!(matches!(update, TelemetryUpdate::Error { .. }));
    }

    #[test]
    fn default_schedule_includes_expected_inquiries() {
        let s = default_schedules();
        assert!(s.contains_key(&Inquiry::Rssi));
        assert!(s.contains_key(&Inquiry::SquelchStatus));
        assert!(s.contains_key(&Inquiry::GeneralStatus));
        assert!(s.contains_key(&Inquiry::Mode));
        assert!(s.contains_key(&Inquiry::PresetSnapshot));
        assert!(s.contains_key(&Inquiry::SynthLock));
        assert_eq!(s[&Inquiry::Rssi], Duration::from_millis(500));
    }

    #[test]
    fn next_due_picks_earliest() {
        let mut schedules = HashMap::new();
        schedules.insert(Inquiry::Rssi, Duration::from_millis(100));
        schedules.insert(Inquiry::SynthLock, Duration::from_secs(5));
        let now = Instant::now();
        let mut last = HashMap::new();
        last.insert(Inquiry::Rssi, now);
        last.insert(Inquiry::SynthLock, now);
        let (_, inq) = next_due_inquiry(&schedules, &last, BackoffMode::Normal).unwrap();
        assert_eq!(inq, Inquiry::Rssi);
    }

    #[test]
    fn idle_backoff_multiplies_period() {
        let mut schedules = HashMap::new();
        schedules.insert(Inquiry::Rssi, Duration::from_millis(100));
        let now = Instant::now();
        let mut last = HashMap::new();
        last.insert(Inquiry::Rssi, now);
        let (due_normal, _) = next_due_inquiry(&schedules, &last, BackoffMode::Normal).unwrap();
        let (due_idle, _) = next_due_inquiry(&schedules, &last, BackoffMode::Idle).unwrap();
        let diff = due_idle.duration_since(due_normal);
        // Idle = 10× normal, so diff ≈ 900ms.
        assert!(diff >= Duration::from_millis(850) && diff <= Duration::from_millis(950));
    }

    #[tokio::test(start_paused = true)]
    async fn poller_emits_rssi_on_schedule() {
        // Script the mock to answer any number of queries with N128+ACK.
        let mut mock = MockTransport::new().lax();
        for _ in 0..20 {
            mock = mock.respond(b"N128\x06");
        }
        let handle = Radio::spawn(mock, Duration::from_millis(100));
        let poller = Poller::spawn(handle.radio.clone());
        // Disable everything except Rssi, to make the test deterministic.
        for inq in [
            Inquiry::SquelchStatus,
            Inquiry::GeneralStatus,
            Inquiry::Mode,
            Inquiry::PresetSnapshot,
            Inquiry::SynthLock,
        ] {
            poller.set_cadence(inq, None).await;
        }
        let mut sub = poller.subscribe();
        // Advance tokio's paused clock to trigger two polls.
        tokio::time::advance(Duration::from_millis(600)).await;
        let first = sub.recv().await.unwrap();
        match first {
            TelemetryUpdate::Rssi(r) => assert_eq!(r.0, 128),
            other => panic!("expected Rssi, got {other:?}"),
        }
        poller.shutdown().await;
        handle.shutdown().await;
    }
}
