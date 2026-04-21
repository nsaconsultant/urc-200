use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serial port error: {0}")]
    Serial(#[from] tokio_serial::Error),
    #[error("transport closed")]
    Closed,
    #[error("mock script exhausted (test bug)")]
    MockExhausted,
    #[error("mock write mismatch: expected {expected:?}, got {actual:?}")]
    MockWriteMismatch {
        expected: Vec<u8>,
        actual: Vec<u8>,
    },
}

#[derive(Debug, Error)]
pub enum RadioError {
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("no response within {0:?}")]
    Timeout(std::time::Duration),
    #[error("three consecutive NAKs — protocol fault")]
    Fault,
    #[error("radio handle closed")]
    Closed,
}
