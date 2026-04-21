use crate::TransportError;
use async_trait::async_trait;

/// Byte-level bidirectional transport to the URC-200.
///
/// Implementors must be cancel-safe on `read`: if a `read` future is dropped
/// before completing, no bytes should be consumed. Both [`SerialTransport`]
/// and [`MockTransport`] satisfy this.
///
/// [`SerialTransport`]: crate::SerialTransport
/// [`MockTransport`]: crate::MockTransport
#[async_trait]
pub trait Transport: Send {
    /// Write exactly `bytes`. Must flush before returning so the bytes are
    /// actually clocked out to the wire.
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Read up to `buf.len()` bytes. Returns the number of bytes actually read,
    /// which is >= 1 on success. A return of `0` means the peer closed (or, for
    /// the mock, the script is exhausted and a `Closed` error is preferable).
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
}
