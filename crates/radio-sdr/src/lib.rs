//! `radio-sdr` — opens an SDR via SoapySDR, streams IQ into an FFT, and
//! broadcasts decimated spectrum frames to any number of subscribers.
//!
//! MVP: single device, single stream, single FFT bin count. No averaging,
//! no max-hold — just a live snapshot at the configured update rate.
//! Designed to drive a browser-side waterfall in HammerHead; the frames are
//! already normalized to u8 so binary WebSocket frames are `bins.len()` bytes.

use anyhow::{Context, Result};
use num_complex::Complex32;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

/// Runtime configuration for the capture thread. `center_hz` and `gain_db`
/// can be updated at runtime via [`SdrCapture::set_center`] and
/// [`SdrCapture::set_gain`]; the thread retunes on its next frame.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SdrConfig {
    /// SoapySDR device args — e.g. `"driver=sdrplay"`.
    pub device_args: String,
    pub center_hz: u64,
    pub sample_rate: u32,
    /// None = auto AGC (recommended default).
    pub gain_db: Option<f64>,
    pub antenna: Option<String>,
    pub fft_size: usize,
    /// Target spectrum frames per second.
    pub update_rate_hz: u32,
}

impl Default for SdrConfig {
    fn default() -> Self {
        Self {
            device_args: "driver=sdrplay".into(),
            center_hz: 251_950_000,
            sample_rate: 2_048_000,
            gain_db: None,
            antenna: None,
            fft_size: 1024,
            update_rate_hz: 30,
        }
    }
}

/// A single FFT snapshot. `bins` is `fft_size` long; bin 0 is the lowest
/// frequency in the span, bin `fft_size - 1` is the highest. Magnitudes are
/// normalized to 0-255 so binary WS frames are one byte per bin.
#[derive(Debug, Clone)]
pub struct SpectrumFrame {
    pub bins: Vec<u8>,
    pub center_hz: u64,
    pub sample_rate: u32,
    pub timestamp_ms: u64,
}

#[derive(Debug)]
enum Cmd {
    SetCenter(u64),
    SetGain(Option<f64>),
    Shutdown,
}

#[derive(Clone)]
pub struct SdrCapture {
    frames: broadcast::Sender<Arc<SpectrumFrame>>,
    cmd: mpsc::Sender<Cmd>,
    current_center: Arc<AtomicU64>,
    sample_rate: Arc<AtomicU32>,
    fft_size: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
}

impl SdrCapture {
    /// Spawn the capture thread. If the device is absent or locked by another
    /// app, the thread retries every 3 s; the handle is usable meanwhile but
    /// [`subscribe`] yields no frames until the device opens.
    pub fn spawn(initial: SdrConfig) -> Self {
        let (frames, _) = broadcast::channel(16);
        let (cmd, cmd_rx) = mpsc::channel(32);
        let current_center = Arc::new(AtomicU64::new(initial.center_hz));
        let sample_rate = Arc::new(AtomicU32::new(initial.sample_rate));
        let fft_size = Arc::new(AtomicU32::new(initial.fft_size as u32));
        let running = Arc::new(AtomicBool::new(true));

        let frames_thread = frames.clone();
        let center_thread = current_center.clone();
        let rate_thread = sample_rate.clone();
        let size_thread = fft_size.clone();
        let run_thread = running.clone();
        std::thread::Builder::new()
            .name("radio-sdr-capture".into())
            .spawn(move || {
                capture_supervisor(
                    initial,
                    cmd_rx,
                    frames_thread,
                    center_thread,
                    rate_thread,
                    size_thread,
                    run_thread,
                );
            })
            .expect("spawn radio-sdr-capture thread");

        Self {
            frames,
            cmd,
            current_center,
            sample_rate,
            fft_size,
            running,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SpectrumFrame>> {
        self.frames.subscribe()
    }

    pub fn center_hz(&self) -> u64 {
        self.current_center.load(Ordering::Relaxed)
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }
    pub fn fft_size(&self) -> usize {
        self.fft_size.load(Ordering::Relaxed) as usize
    }

    pub async fn set_center(&self, hz: u64) {
        let _ = self.cmd.send(Cmd::SetCenter(hz)).await;
    }
    pub async fn set_gain(&self, db: Option<f64>) {
        let _ = self.cmd.send(Cmd::SetGain(db)).await;
    }
    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.cmd.send(Cmd::Shutdown).await;
    }
}

fn capture_supervisor(
    initial: SdrConfig,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    frames: broadcast::Sender<Arc<SpectrumFrame>>,
    current_center: Arc<AtomicU64>,
    sample_rate: Arc<AtomicU32>,
    fft_size: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
) {
    let mut cfg = initial;
    while running.load(Ordering::Relaxed) {
        match run_capture(
            &cfg,
            &mut cmd_rx,
            &frames,
            &current_center,
            &sample_rate,
            &fft_size,
            &running,
        ) {
            Ok(next_cfg) => {
                // Returned normally due to a runtime config change. Loop with
                // the new config to re-open the device.
                cfg = next_cfg;
            }
            Err(e) => {
                warn!(error = ?e, "sdr capture failed — retrying in 3s");
                // Carry the latest retuned center over into the retry. Without
                // this, an SDR read stall right after the user hit "SDR → radio"
                // would bounce the viewer back to `initial.center_hz`.
                cfg.center_hz = current_center.load(Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
    info!("sdr capture supervisor exiting");
}

fn run_capture(
    cfg: &SdrConfig,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
    frames: &broadcast::Sender<Arc<SpectrumFrame>>,
    current_center: &AtomicU64,
    sample_rate: &AtomicU32,
    fft_size: &AtomicU32,
    running: &AtomicBool,
) -> Result<SdrConfig> {
    info!(
        device = %cfg.device_args,
        center_hz = cfg.center_hz,
        sample_rate = cfg.sample_rate,
        fft_size = cfg.fft_size,
        update_rate_hz = cfg.update_rate_hz,
        "opening SDR"
    );
    let dev = soapysdr::Device::new(&*cfg.device_args)
        .with_context(|| format!("open soapysdr device {:?}", cfg.device_args))?;
    dev.set_sample_rate(soapysdr::Direction::Rx, 0, cfg.sample_rate as f64)
        .context("set_sample_rate")?;
    dev.set_frequency(soapysdr::Direction::Rx, 0, cfg.center_hz as f64, "")
        .context("set_frequency")?;
    match cfg.gain_db {
        Some(g) => {
            let _ = dev.set_gain_mode(soapysdr::Direction::Rx, 0, false);
            dev.set_gain(soapysdr::Direction::Rx, 0, g).context("set_gain")?;
        }
        None => {
            let _ = dev.set_gain_mode(soapysdr::Direction::Rx, 0, true);
        }
    }
    if let Some(ant) = &cfg.antenna {
        let _ = dev.set_antenna(soapysdr::Direction::Rx, 0, ant.as_str());
    }

    let mut rx = dev
        .rx_stream::<Complex32>(&[0])
        .context("open rx_stream")?;
    rx.activate(None).context("activate rx_stream")?;
    info!("rx stream activated");

    current_center.store(cfg.center_hz, Ordering::Relaxed);
    sample_rate.store(cfg.sample_rate, Ordering::Relaxed);
    fft_size.store(cfg.fft_size as u32, Ordering::Relaxed);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(cfg.fft_size);

    // Hann window, precomputed.
    let window: Vec<f32> = (0..cfg.fft_size)
        .map(|i| {
            0.5 - 0.5 * ((2.0 * std::f32::consts::PI * i as f32) / (cfg.fft_size as f32 - 1.0)).cos()
        })
        .collect();
    let win_sum: f32 = window.iter().sum();
    // Coherent gain compensation for normalized output.
    let norm = 1.0 / (win_sum / cfg.fft_size as f32);

    let samples_per_update = (cfg.sample_rate as usize) / (cfg.update_rate_hz.max(1) as usize);
    let mut history: VecDeque<Complex32> = VecDeque::with_capacity(cfg.fft_size);
    let mut samples_since_last = 0usize;

    // Working buffers.
    let mut buf = vec![Complex32::new(0.0, 0.0); 16384];
    let mut fft_buf: Vec<Complex32> = vec![Complex32::new(0.0, 0.0); cfg.fft_size];

    while running.load(Ordering::Relaxed) {
        // Drain control messages non-blockingly.
        while let Ok(msg) = cmd_rx.try_recv() {
            match msg {
                Cmd::SetCenter(hz) => {
                    if let Err(e) = dev.set_frequency(soapysdr::Direction::Rx, 0, hz as f64, "") {
                        warn!(error = ?e, "set_frequency failed");
                    } else {
                        current_center.store(hz, Ordering::Relaxed);
                    }
                }
                Cmd::SetGain(None) => {
                    let _ = dev.set_gain_mode(soapysdr::Direction::Rx, 0, true);
                }
                Cmd::SetGain(Some(g)) => {
                    let _ = dev.set_gain_mode(soapysdr::Direction::Rx, 0, false);
                    if let Err(e) = dev.set_gain(soapysdr::Direction::Rx, 0, g) {
                        warn!(error = ?e, "set_gain failed");
                    }
                }
                Cmd::Shutdown => {
                    rx.deactivate(None).ok();
                    return Ok(cfg.clone());
                }
            }
        }

        let n = match rx.read(&mut [&mut buf], 200_000) {
            Ok(n) => n,
            Err(e) => {
                warn!(error = ?e, "rx_stream read error — restarting");
                rx.deactivate(None).ok();
                return Err(anyhow::anyhow!("rx read: {e:?}"));
            }
        };

        for &sample in &buf[..n] {
            if history.len() >= cfg.fft_size {
                history.pop_front();
            }
            history.push_back(sample);
            samples_since_last += 1;

            if samples_since_last >= samples_per_update && history.len() == cfg.fft_size {
                samples_since_last = 0;

                for (i, &c) in history.iter().enumerate() {
                    fft_buf[i] = Complex32::new(c.re * window[i], c.im * window[i]);
                }
                fft.process(&mut fft_buf);

                // Magnitudes → dB → u8, with fftshift (negative freqs first).
                let mid = cfg.fft_size / 2;
                let mut bins = Vec::with_capacity(cfg.fft_size);
                for &c in fft_buf.iter().skip(mid) {
                    bins.push(mag_to_u8(c, norm));
                }
                for &c in fft_buf.iter().take(mid) {
                    bins.push(mag_to_u8(c, norm));
                }

                let frame = Arc::new(SpectrumFrame {
                    bins,
                    center_hz: current_center.load(Ordering::Relaxed),
                    sample_rate: cfg.sample_rate,
                    timestamp_ms: now_ms(),
                });
                let _ = frames.send(frame);
            }
        }
    }
    rx.deactivate(None).ok();
    Ok(cfg.clone())
}

#[inline]
fn mag_to_u8(c: Complex32, norm: f32) -> u8 {
    let mag = (c.re * c.re + c.im * c.im).sqrt() * norm;
    let db = 20.0 * (mag + 1e-12).log10();
    // Noise floor around -100 dB maps to 0; -20 dB maps near 255.
    let scaled = ((db + 100.0) * (255.0 / 80.0)).clamp(0.0, 255.0);
    scaled as u8
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
