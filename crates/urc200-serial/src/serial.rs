use crate::{Transport, TransportError};
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, SerialStream, StopBits};

/// Serial-port settings. Defaults match URC-200 §4.6.2: 1200 bps, 8-N-1, no flow control.
#[derive(Debug, Clone)]
pub struct SerialConfig {
    pub path: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub stop_bits: StopBits,
    pub parity: Parity,
    pub flow_control: FlowControl,
    pub open_timeout: Duration,
}

impl SerialConfig {
    pub fn urc200(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            baud: 1200,
            data_bits: DataBits::Eight,
            stop_bits: StopBits::One,
            parity: Parity::None,
            flow_control: FlowControl::None,
            open_timeout: Duration::from_millis(100),
        }
    }
}

/// Real `tokio-serial`-backed [`Transport`].
pub struct SerialTransport {
    stream: SerialStream,
}

impl SerialTransport {
    /// Open a serial port configured for the URC-200.
    pub fn open(cfg: &SerialConfig) -> Result<Self, TransportError> {
        let stream = tokio_serial::new(&cfg.path, cfg.baud)
            .data_bits(cfg.data_bits)
            .stop_bits(cfg.stop_bits)
            .parity(cfg.parity)
            .flow_control(cfg.flow_control)
            .timeout(cfg.open_timeout)
            .open_native_async()?;
        Ok(Self { stream })
    }
}

#[async_trait]
impl Transport for SerialTransport {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let n = self.stream.read(buf).await?;
        Ok(n)
    }
}
