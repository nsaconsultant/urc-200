use crate::{Transport, TransportError};
use async_trait::async_trait;
use std::collections::VecDeque;

/// In-memory transport for deterministic unit tests.
///
/// Build a mock by chaining expectations:
/// ```
/// use urc200_serial::{MockTransport, Transport};
/// # async fn example() {
/// let mut mock = MockTransport::new()
///     .expect_write(b"P3")
///     .respond(b"ACK");
/// mock.write_all(b"P3").await.unwrap();
/// let mut buf = [0u8; 8];
/// let n = mock.read(&mut buf).await.unwrap();
/// assert_eq!(&buf[..n], b"ACK");
/// # }
/// ```
///
/// Expectations are consumed in order. If you want to assert on writes without
/// being strict about order, record to `sent()` and inspect after the test.
pub struct MockTransport {
    script: VecDeque<Step>,
    sent: Vec<u8>,
    rx_queue: VecDeque<u8>,
    /// If true, strict write matching against `ExpectWrite` steps is enforced.
    /// If false, writes pass through silently and only `Respond` steps gate reads.
    strict: bool,
    /// If true, `read()` blocks forever when there's nothing to return instead of
    /// erroring with `Closed`. Useful for timeout tests that need to simulate a
    /// powered-on-but-unresponsive radio.
    silent_on_empty: bool,
}

#[derive(Debug, Clone)]
enum Step {
    ExpectWrite(Vec<u8>),
    Respond(Vec<u8>),
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    pub fn new() -> Self {
        Self {
            script: VecDeque::new(),
            sent: Vec::new(),
            rx_queue: VecDeque::new(),
            strict: true,
            silent_on_empty: false,
        }
    }

    /// Disable strict write matching — writes are recorded but not asserted.
    /// Useful when the test cares only about how the caller handles responses.
    pub fn lax(mut self) -> Self {
        self.strict = false;
        self
    }

    /// Read blocks forever when there's nothing scripted. Simulates a radio
    /// that's powered on and listening but never answers — the case the
    /// dispatcher's timeout logic is designed for.
    pub fn silent(mut self) -> Self {
        self.silent_on_empty = true;
        self
    }

    /// Add a step: next `write_all` must exactly equal `bytes`.
    pub fn expect_write(mut self, bytes: &[u8]) -> Self {
        self.script.push_back(Step::ExpectWrite(bytes.to_vec()));
        self
    }

    /// Add a step: next `read` will deliver these bytes (possibly across
    /// multiple read calls, depending on the buffer size the caller provides).
    pub fn respond(mut self, bytes: &[u8]) -> Self {
        self.script.push_back(Step::Respond(bytes.to_vec()));
        self
    }

    /// Bytes the caller has written, in order.
    pub fn sent(&self) -> &[u8] {
        &self.sent
    }

    /// Drop any remaining scripted steps. Useful before re-scripting a mock.
    pub fn reset_script(&mut self) {
        self.script.clear();
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.sent.extend_from_slice(bytes);
        if !self.strict {
            return Ok(());
        }
        // Pop the next ExpectWrite, ignoring Respond steps so writes and reads
        // can interleave without order brittleness.
        while let Some(front) = self.script.front() {
            match front {
                Step::Respond(_) => break,
                Step::ExpectWrite(expected) => {
                    let expected = expected.clone();
                    self.script.pop_front();
                    if expected != bytes {
                        return Err(TransportError::MockWriteMismatch {
                            expected,
                            actual: bytes.to_vec(),
                        });
                    }
                    return Ok(());
                }
            }
        }
        // No ExpectWrite queued — in strict mode, this is a test bug.
        Err(TransportError::MockExhausted)
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // Pull more bytes into the rx_queue from the next Respond step if empty.
        if self.rx_queue.is_empty() {
            while let Some(front) = self.script.front() {
                match front {
                    Step::ExpectWrite(_) => {
                        // Reads don't consume ExpectWrite steps.
                        break;
                    }
                    Step::Respond(bytes) => {
                        let bytes = bytes.clone();
                        self.script.pop_front();
                        self.rx_queue.extend(bytes);
                        break;
                    }
                }
            }
        }
        if self.rx_queue.is_empty() {
            if self.silent_on_empty {
                // Simulate a silent-but-alive peer: block forever. The caller's
                // timeout will fire.
                std::future::pending::<()>().await;
            }
            return Err(TransportError::Closed);
        }
        let n = buf.len().min(self.rx_queue.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.rx_queue.pop_front().unwrap();
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let mut m = MockTransport::new()
            .expect_write(b"Z")
            .respond(b"ACK");
        m.write_all(b"Z").await.unwrap();
        let mut buf = [0u8; 16];
        let n = m.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ACK");
        assert_eq!(m.sent(), b"Z");
    }

    #[tokio::test]
    async fn strict_write_mismatch_errors() {
        let mut m = MockTransport::new().expect_write(b"P3");
        let err = m.write_all(b"P4").await.unwrap_err();
        assert!(matches!(err, TransportError::MockWriteMismatch { .. }));
    }

    #[tokio::test]
    async fn lax_mode_accepts_any_write() {
        let mut m = MockTransport::new().lax().respond(b"ACK");
        m.write_all(b"anything").await.unwrap();
        m.write_all(b"else").await.unwrap();
        assert_eq!(m.sent(), b"anythingelse");
    }

    #[tokio::test]
    async fn read_across_multiple_reads() {
        let mut m = MockTransport::new().respond(b"ACKACK");
        let mut buf = [0u8; 3];
        let n = m.read(&mut buf).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..], b"ACK");
        let n = m.read(&mut buf).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..], b"ACK");
    }

    #[tokio::test]
    async fn interleaved_write_and_read() {
        let mut m = MockTransport::new()
            .expect_write(b"?03")
            .respond(b"N128ACK")
            .expect_write(b"L2")
            .respond(b"ACK");
        m.write_all(b"?03").await.unwrap();
        let mut buf = [0u8; 16];
        let n = m.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"N128ACK");
        m.write_all(b"L2").await.unwrap();
        let n = m.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ACK");
    }

    #[tokio::test]
    async fn exhausted_read_returns_closed() {
        let mut m = MockTransport::new().respond(b"X");
        let mut buf = [0u8; 4];
        let _ = m.read(&mut buf).await.unwrap();
        let err = m.read(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }
}
