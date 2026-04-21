//! Small audio DSP: biquad HPF/LPF + RMS noise gate.
//!
//! Target use is narrowband voice (UAA RX stream / browser mic TX). Filters
//! default to a 300-3000 Hz band and a mild noise gate. All parameters are
//! live-tunable via the shared `FilterConfig`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

// ======================== Biquad (RBJ cookbook) ========================

#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn bypass() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z1: 0.0, z2: 0.0 }
    }
    pub fn lowpass(sr: f32, fc: f32, q: f32) -> Self {
        let (b0, b1, b2, a1, a2) = lp_coefs(sr, fc.max(20.0).min(sr / 2.0 - 100.0), q);
        Self { b0, b1, b2, a1, a2, z1: 0.0, z2: 0.0 }
    }
    pub fn highpass(sr: f32, fc: f32, q: f32) -> Self {
        let (b0, b1, b2, a1, a2) = hp_coefs(sr, fc.max(20.0).min(sr / 2.0 - 100.0), q);
        Self { b0, b1, b2, a1, a2, z1: 0.0, z2: 0.0 }
    }
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // Direct Form II Transposed
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
    /// Reset internal state without changing coefficients.
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

fn lp_coefs(sr: f32, fc: f32, q: f32) -> (f32, f32, f32, f32, f32) {
    let w = 2.0 * std::f32::consts::PI * fc / sr;
    let cs = w.cos();
    let alpha = w.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    (
        ((1.0 - cs) / 2.0) / a0,
        (1.0 - cs) / a0,
        ((1.0 - cs) / 2.0) / a0,
        (-2.0 * cs) / a0,
        (1.0 - alpha) / a0,
    )
}

fn hp_coefs(sr: f32, fc: f32, q: f32) -> (f32, f32, f32, f32, f32) {
    let w = 2.0 * std::f32::consts::PI * fc / sr;
    let cs = w.cos();
    let alpha = w.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    (
        ((1.0 + cs) / 2.0) / a0,
        (-(1.0 + cs)) / a0,
        ((1.0 + cs) / 2.0) / a0,
        (-2.0 * cs) / a0,
        (1.0 - alpha) / a0,
    )
}

// ======================== Noise gate ========================

#[derive(Debug, Clone, Copy)]
pub struct NoiseGate {
    threshold: f32,   // normalized 0..1 (RMS of |x|)
    attack: f32,      // envelope per-sample coefficient (close to 1)
    release: f32,
    env: f32,         // current envelope of |x|
    gain: f32,        // current output gain (0..1), smoothed
}

impl NoiseGate {
    pub fn new(sr: f32, threshold_db: f32) -> Self {
        let threshold = 10f32.powf(threshold_db / 20.0); // e.g., -40 dB → 0.01
        // attack 10 ms, release 80 ms (per-sample exponential)
        let attack = (-1.0 / (sr * 0.010)).exp();
        let release = (-1.0 / (sr * 0.080)).exp();
        Self { threshold, attack, release, env: 0.0, gain: 0.0 }
    }
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // One-pole envelope on |x| with asymmetric attack/release
        let abs_x = x.abs();
        if abs_x > self.env {
            self.env = self.attack * (self.env - abs_x) + abs_x;
        } else {
            self.env = self.release * (self.env - abs_x) + abs_x;
        }
        let target = if self.env > self.threshold { 1.0 } else { 0.0 };
        // Smooth gain with same release coefficient (avoids clicks)
        self.gain = self.release * (self.gain - target) + target;
        x * self.gain
    }
    pub fn reconfigure(&mut self, sr: f32, threshold_db: f32) {
        self.threshold = 10f32.powf(threshold_db / 20.0);
        self.attack = (-1.0 / (sr * 0.010)).exp();
        self.release = (-1.0 / (sr * 0.080)).exp();
    }
}

// ======================== Live-config handle ========================

/// Shared, atomics-backed filter parameters. Updated from HTTP handlers;
/// the audio thread polls the generation counter at the top of each block.
#[derive(Clone)]
pub struct FilterConfig {
    pub hp_enabled: Arc<AtomicBool>,
    pub lp_enabled: Arc<AtomicBool>,
    pub gate_enabled: Arc<AtomicBool>,
    pub hp_fc: Arc<AtomicU32>,    // Hz × 10 (so 300 Hz = 3000)
    pub lp_fc: Arc<AtomicU32>,
    pub gate_db: Arc<AtomicU32>,  // (-dB) × 10 — e.g. 400 means -40 dB
    pub generation: Arc<AtomicU64>,
}

impl FilterConfig {
    pub fn new_voice_defaults() -> Self {
        let f = Self {
            hp_enabled: Arc::new(AtomicBool::new(true)),
            lp_enabled: Arc::new(AtomicBool::new(true)),
            gate_enabled: Arc::new(AtomicBool::new(false)),
            hp_fc: Arc::new(AtomicU32::new(3000)),   // 300.0 Hz
            lp_fc: Arc::new(AtomicU32::new(30000)),  // 3000.0 Hz
            gate_db: Arc::new(AtomicU32::new(400)),  // -40.0 dB
            generation: Arc::new(AtomicU64::new(1)),
        };
        f
    }
    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> FilterSnapshot {
        FilterSnapshot {
            hp_enabled: self.hp_enabled.load(Ordering::Relaxed),
            lp_enabled: self.lp_enabled.load(Ordering::Relaxed),
            gate_enabled: self.gate_enabled.load(Ordering::Relaxed),
            hp_fc: self.hp_fc.load(Ordering::Relaxed) as f32 / 10.0,
            lp_fc: self.lp_fc.load(Ordering::Relaxed) as f32 / 10.0,
            gate_db: -(self.gate_db.load(Ordering::Relaxed) as f32 / 10.0),
            generation: self.generation.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FilterSnapshot {
    pub hp_enabled: bool,
    pub lp_enabled: bool,
    pub gate_enabled: bool,
    pub hp_fc: f32,
    pub lp_fc: f32,
    pub gate_db: f32,
    #[serde(default)]
    pub generation: u64,
}

// ======================== Thread-local chain ========================

/// Lives on the audio thread. Applies HPF → gate → LPF in place on i16 mono samples.
pub struct FilterChain {
    sr: f32,
    hp: Biquad,
    lp: Biquad,
    gate: NoiseGate,
    applied_gen: u64,
    snap: FilterSnapshot,
}

impl FilterChain {
    pub fn new(sr: f32, cfg: &FilterConfig) -> Self {
        let snap = cfg.snapshot();
        let hp = Biquad::highpass(sr, snap.hp_fc, std::f32::consts::FRAC_1_SQRT_2);
        let lp = Biquad::lowpass(sr, snap.lp_fc, std::f32::consts::FRAC_1_SQRT_2);
        let gate = NoiseGate::new(sr, snap.gate_db);
        Self { sr, hp, lp, gate, applied_gen: snap.generation, snap }
    }
    pub fn maybe_reconfigure(&mut self, cfg: &FilterConfig) {
        let gen = cfg.generation.load(Ordering::Relaxed);
        if gen == self.applied_gen {
            return;
        }
        self.snap = cfg.snapshot();
        self.hp = Biquad::highpass(self.sr, self.snap.hp_fc, std::f32::consts::FRAC_1_SQRT_2);
        self.lp = Biquad::lowpass(self.sr, self.snap.lp_fc, std::f32::consts::FRAC_1_SQRT_2);
        self.gate.reconfigure(self.sr, self.snap.gate_db);
        self.applied_gen = gen;
    }
    pub fn process_mono_i16(&mut self, samples: &mut [i16]) {
        let hp_on = self.snap.hp_enabled;
        let lp_on = self.snap.lp_enabled;
        let g_on = self.snap.gate_enabled;
        for s in samples.iter_mut() {
            let mut x = *s as f32;
            if hp_on { x = self.hp.process(x); }
            if g_on  { x = self.gate.process(x); }
            if lp_on { x = self.lp.process(x); }
            *s = x.clamp(-32768.0, 32767.0) as i16;
        }
    }
}
