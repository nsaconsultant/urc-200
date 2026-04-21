//! UAA audio capture → WebSocket fanout.
//!
//! Captures `hw:UAA2,0` at 16 kHz mono S16LE (the narrowband-voice sweet spot
//! for the URC-200; keeps bandwidth to 256 kbps over the WebSocket) and
//! broadcasts raw PCM frames to any number of browser subscribers.
//!
//! The capture runs on a dedicated thread — ALSA is blocking and we don't want
//! to starve the tokio runtime. The broadcast channel is `i16` samples; the WS
//! handler converts each frame to little-endian bytes before sending.
//!
//! Opus compression is not done here; see `S-110 follow-up` to add it later.
//! Raw PCM is trivial for the browser to decode (`AudioBuffer.copyToChannel`)
//! and the LAN/Tailscale path has plenty of headroom.

use crate::dsp::{FilterChain, FilterConfig};
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

/// Desired capture rate — we ask for this but accept whatever the device
/// negotiates. The actual rate is reported back to the client in the
/// `audio_header` JSON frame so the browser uses the right AudioBuffer rate.
/// UAA's AC'97 codec runs at 48 kHz natively so that's what we typically get.
pub const PREFERRED_SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 1;
/// 10 ms frame target; adjusted to match whatever the device picks.
pub const PERIOD_MS: u32 = 10;
const BROADCAST_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct AudioCapture {
    samples: broadcast::Sender<Arc<Vec<i16>>>,
    running: Arc<AtomicBool>,
    actual_rate: Arc<AtomicU32>,
    pub filters: FilterConfig,
}

impl AudioCapture {
    /// Spawn the capture thread. Returns a handle whose `subscribe()` yields
    /// a fresh `broadcast::Receiver`. The capture keeps trying to open the
    /// device if it's absent at startup (useful for Docker: the container may
    /// come up before the USB device is reattached).
    pub fn spawn(device: String) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let running = Arc::new(AtomicBool::new(true));
        let actual_rate = Arc::new(AtomicU32::new(0));
        let filters = FilterConfig::new_voice_defaults();
        let tx_thread = tx.clone();
        let run_thread = running.clone();
        let rate_thread = actual_rate.clone();
        let filters_thread = filters.clone();
        let dev = device.clone();
        thread::Builder::new()
            .name("uaa-capture".into())
            .spawn(move || {
                while run_thread.load(Ordering::Relaxed) {
                    match capture_once(&dev, &tx_thread, &run_thread, &rate_thread, &filters_thread) {
                        Ok(()) => {
                            info!(device = %dev, "capture thread exited cleanly");
                            break;
                        }
                        Err(e) => {
                            warn!(device = %dev, error = ?e, "capture failed; retrying in 2s");
                            thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            })
            .expect("spawn uaa-capture thread");
        Self {
            samples: tx,
            running,
            actual_rate,
            filters,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Vec<i16>>> {
        self.samples.subscribe()
    }

    pub fn actual_rate(&self) -> u32 {
        self.actual_rate.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// ========================================================================
// TX direction: browser mic → UAA playback (radio mic input)
// ========================================================================

/// Software CTCSS tone generator. Cheap to clone; mixing is done in the
/// playback thread, config is set from HTTP.
///
/// Default amplitude is ~10% of peak i16 (3200). The URC-200's base-band
/// firmware has no CTCSS encoder of its own and the PT audio input may have
/// a high-pass filter that attenuates sub-audible tones — if repeaters don't
/// open, `POST /api/tx/ctcss` with a higher amplitude (`level` field, 0-32767)
/// to bump it.
#[derive(Clone)]
pub struct CtcssMixer {
    enabled: Arc<AtomicBool>,
    freq_centihz: Arc<AtomicU32>, // store Hz × 100 so 162.2 Hz is represented exactly
    amplitude: Arc<AtomicU32>,    // 0..32767
}

impl CtcssMixer {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            freq_centihz: Arc::new(AtomicU32::new(0)),
            amplitude: Arc::new(AtomicU32::new(3200)), // ~10% default
        }
    }
    pub fn set(&self, hz: Option<f32>) {
        match hz {
            Some(f) if f > 30.0 && f < 300.0 => {
                self.freq_centihz
                    .store((f * 100.0).round() as u32, Ordering::Relaxed);
                self.enabled.store(true, Ordering::Relaxed);
            }
            _ => self.enabled.store(false, Ordering::Relaxed),
        }
    }
    pub fn set_amplitude(&self, amp: u16) {
        self.amplitude.store(amp.min(32767) as u32, Ordering::Relaxed);
    }
    pub fn get(&self) -> (bool, f32, u16) {
        (
            self.enabled.load(Ordering::Relaxed),
            self.freq_centihz.load(Ordering::Relaxed) as f32 / 100.0,
            self.amplitude.load(Ordering::Relaxed) as u16,
        )
    }
}

/// Per-thread mix state — owned by the playback loop only.
struct CtcssPhase(f32);

impl CtcssPhase {
    fn new() -> Self { Self(0.0) }
    fn mix_into(&mut self, mixer: &CtcssMixer, samples: &mut [i16], sample_rate: u32) {
        if !mixer.enabled.load(Ordering::Relaxed) {
            return;
        }
        let freq = mixer.freq_centihz.load(Ordering::Relaxed) as f32 / 100.0;
        if freq <= 0.0 {
            return;
        }
        let amp = mixer.amplitude.load(Ordering::Relaxed) as f32;
        let two_pi = std::f32::consts::TAU;
        let step = two_pi * freq / sample_rate as f32;
        for s in samples.iter_mut() {
            let tone = (self.0.sin() * amp) as i32;
            let mixed = (*s as i32) + tone;
            *s = mixed.clamp(-32768, 32767) as i16;
            self.0 += step;
            if self.0 > two_pi {
                self.0 -= two_pi;
            }
        }
    }
}

/// Handle to the TX playback thread. Cheap to clone.
#[derive(Clone)]
pub struct AudioTx {
    tx: mpsc::Sender<Vec<i16>>,
    actual_rate: Arc<AtomicU32>,
    pub ctcss: CtcssMixer,
    pub filters: FilterConfig,
}

impl AudioTx {
    /// Spawn a blocking thread that writes to the UAA playback PCM.
    pub fn spawn(device: String) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<i16>>(256);
        let actual_rate = Arc::new(AtomicU32::new(0));
        let ctcss = CtcssMixer::new();
        let filters = FilterConfig::new_voice_defaults();
        let dev = device.clone();
        let rate_thread = actual_rate.clone();
        let ctcss_thread = ctcss.clone();
        let filters_thread = filters.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = tx_playback_loop(&dev, rx, &rate_thread, &ctcss_thread, &filters_thread) {
                warn!(device = %dev, error = ?e, "tx playback failed");
            }
        });
        Self {
            tx,
            actual_rate,
            ctcss,
            filters,
        }
    }

    /// Non-blocking send. Drops samples if the ALSA writer can't keep up
    /// (better than blocking the WebSocket task).
    pub fn try_send(&self, samples: Vec<i16>) -> bool {
        self.tx.try_send(samples).is_ok()
    }

    pub fn actual_rate(&self) -> u32 {
        self.actual_rate.load(Ordering::Relaxed)
    }
}

fn tx_playback_loop(
    device: &str,
    mut rx: mpsc::Receiver<Vec<i16>>,
    rate_out: &AtomicU32,
    ctcss: &CtcssMixer,
    filters: &FilterConfig,
) -> Result<()> {
    let pcm = PCM::new(device, Direction::Playback, false)
        .with_context(|| format!("open {device} for playback"))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_rate_near(PREFERRED_SAMPLE_RATE, ValueOr::Nearest)?;
        if hwp.set_channels(CHANNELS).is_err() {
            hwp.set_channels_near(CHANNELS)?;
        }
        let rate_so_far = hwp.get_rate().unwrap_or(PREFERRED_SAMPLE_RATE);
        let period_frames = (rate_so_far as u32 * PERIOD_MS / 1000) as alsa::pcm::Frames;
        hwp.set_period_size_near(period_frames, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    let actual_rate = pcm.hw_params_current()?.get_rate()?;
    let actual_channels = pcm.hw_params_current()?.get_channels()? as usize;
    let buffer_size = pcm.hw_params_current()?.get_buffer_size()?;
    rate_out.store(actual_rate, Ordering::Relaxed);
    info!(
        device,
        actual_rate, actual_channels, buffer_size, "tx playback started"
    );

    let io = pcm.io_i16()?;
    pcm.prepare()?;

    // Prime with silence so ALSA starts cleanly on first real write.
    let silence = vec![0i16; buffer_size as usize * actual_channels];
    let _ = io.writei(&silence);

    let mut ctcss_phase = CtcssPhase::new();
    let mut tx_filter = FilterChain::new(actual_rate as f32, filters);
    while let Some(mut mono_samples) = rx.blocking_recv() {
        // 1. Clean up mic audio (HPF removes rumble/hum, LPF caps the voice
        //    band at ~3 kHz, optional noise gate cuts dead-air hiss).
        tx_filter.maybe_reconfigure(filters);
        tx_filter.process_mono_i16(&mut mono_samples);
        // 2. Mix CTCSS tone (if enabled) AFTER filtering so the sub-audible
        //    tone isn't eaten by the high-pass.
        ctcss_phase.mix_into(ctcss, &mut mono_samples, actual_rate);
        // Incoming is always mono (browser constraint). Expand to whatever
        // number of channels the device negotiated.
        let to_write: Vec<i16> = if actual_channels == 1 {
            mono_samples
        } else {
            let mut out = Vec::with_capacity(mono_samples.len() * actual_channels);
            for s in mono_samples {
                for _ in 0..actual_channels {
                    out.push(s);
                }
            }
            out
        };
        match io.writei(&to_write) {
            Ok(_) => {}
            Err(e) => {
                warn!(error = ?e, "tx xrun");
                let _ = pcm.try_recover(e, true);
            }
        }
    }
    info!("tx playback loop ended");
    Ok(())
}

// ========================================================================

fn capture_once(
    device: &str,
    tx: &broadcast::Sender<Arc<Vec<i16>>>,
    running: &AtomicBool,
    rate_out: &AtomicU32,
    filters: &FilterConfig,
) -> Result<()> {
    let pcm = PCM::new(device, Direction::Capture, false)
        .with_context(|| format!("open {device} for capture"))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        // UAA is AC'97 @ 48 kHz native; accept whatever the device supports.
        hwp.set_rate_near(PREFERRED_SAMPLE_RATE, ValueOr::Nearest)?;
        if hwp.set_channels(CHANNELS).is_err() {
            hwp.set_channels_near(CHANNELS)?;
        }
        let rate_so_far = hwp.get_rate().unwrap_or(PREFERRED_SAMPLE_RATE);
        let period_frames = (rate_so_far as u32 * PERIOD_MS / 1000) as alsa::pcm::Frames;
        hwp.set_period_size_near(period_frames, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    let actual_rate = pcm.hw_params_current()?.get_rate()?;
    let actual_channels = pcm.hw_params_current()?.get_channels()?;
    let period_frames = pcm.hw_params_current()?.get_period_size()?;
    rate_out.store(actual_rate, Ordering::Relaxed);
    info!(device, actual_rate, actual_channels, period_frames, "capture started");

    let io = pcm.io_i16()?;
    pcm.prepare()?;
    pcm.start()?;

    let mut buf = vec![0i16; period_frames as usize * actual_channels as usize];
    let mut rx_filter = FilterChain::new(actual_rate as f32, filters);
    while running.load(Ordering::Relaxed) {
        match io.readi(&mut buf) {
            Ok(_) => {
                // Down-mix to mono if the device gave us >1 channel.
                let mut mono: Vec<i16> = if actual_channels == 1 {
                    buf.clone()
                } else {
                    buf.chunks_exact(actual_channels as usize)
                        .map(|frame| {
                            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
                            (sum / actual_channels as i32) as i16
                        })
                        .collect()
                };
                rx_filter.maybe_reconfigure(filters);
                rx_filter.process_mono_i16(&mut mono);
                let _ = tx.send(Arc::new(mono));
            }
            Err(e) => {
                warn!(error = ?e, "xrun");
                pcm.try_recover(e, true)?;
            }
        }
    }
    Ok(())
}
