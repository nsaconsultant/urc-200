//! URC-200 (V2) serial protocol codec.
//!
//! Encodes Table 11 remote-operation commands (§4.6.4) to the ASCII wire
//! format the radio expects over RS-232 at 1200 bps, 8-N-1.
//!
//! No response decoding yet — that lands in S-011 (ACK/NAK/HT FSM) and S-012
//! (Table 13 status inquiries).

pub mod command;
pub mod freq;
pub mod inquiry;
pub mod response;

pub use command::{
    LampLevel, ModMode, OpCommand, PowerLevel, PresetId, TextMode, ToneSquelch,
};
pub use freq::{Band, Freq, FreqError, Step};
pub use inquiry::{
    GeneralStatus, Inquiry, Mode, PresetSnapshot, Rssi, SquelchStatus, SynthLock,
};
pub use response::{DispatchOutcome, NakCounter, Response, ResponseParser};
