//! Byte-pipe transport for the URC-200 RS-232 link.
//!
//! This crate is deliberately dumb: it moves bytes between the PC and the
//! radio. Command framing, ACK/NAK accounting, and timeouts live in the
//! dispatcher (S-021). Both the real serial port and the test mock implement
//! [`Transport`] so callers can be generic.

mod error;
mod mock;
mod poller;
mod radio;
mod serial;
mod transport;

pub use error::{RadioError, TransportError};
pub use mock::MockTransport;
pub use poller::{BackoffMode, PollErrorKind, Poller, TelemetryUpdate};
pub use radio::{Radio, RadioHandle, DEFAULT_TIMEOUT};
pub use serial::{SerialConfig, SerialTransport};
pub use transport::Transport;
