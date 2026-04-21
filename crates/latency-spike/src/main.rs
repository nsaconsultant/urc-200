// S-001: UAA round-trip latency spike
//
// Measures the floor latency through the Linux USB audio stack by opening
// `hw:UAA2,0` directly via ALSA, bypassing PipeWire's userland. The production
// app uses cpal-via-PipeWire; any PipeWire overhead is additive and measured
// later.
//
// Measures:
//   - Stream-up latency
//   - Capture/playback loop interval jitter (always)
//   - Round-trip tone-burst latency (requires physical loopback on UAA)
//
// Gate (Winston): round-trip p99 < 80 ms.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEVICE_DEFAULT: &str = "hw:UAA2,0";
const SAMPLE_RATE: u32 = 48_000;
const PERIOD_FRAMES: i32 = 256;
const BUFFER_FRAMES: i32 = 1024;
const TONE_HZ: f32 = 1000.0;
const TONE_DUR_MS: u64 = 20;
const BURST_PERIOD_MS: u64 = 500;
const DETECT_RMS_THRESHOLD: f32 = 0.02;
const RUN_SECS_DEFAULT: u64 = 600;
const MAX_PLAUSIBLE_RTT: Duration = Duration::from_millis(1000);

fn main() -> Result<()> {
    let run_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(RUN_SECS_DEFAULT);
    let device: String = std::env::var("URC_UAA_DEVICE").unwrap_or_else(|_| DEVICE_DEFAULT.into());

    println!("Device: {device}");

    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        ctrlc::set_handler(move || s.store(true, Ordering::SeqCst))?;
    }

    let burst_emit_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let rtt_samples: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let in_loop_ts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let out_loop_ts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let tone_state: Arc<Mutex<ToneState>> = Arc::new(Mutex::new(ToneState::default()));

    let start = Instant::now();

    let playback_thread = spawn_playback(
        device.clone(),
        stop.clone(),
        tone_state.clone(),
        out_loop_ts.clone(),
    );

    let capture_thread = spawn_capture(
        device.clone(),
        stop.clone(),
        burst_emit_at.clone(),
        rtt_samples.clone(),
        in_loop_ts.clone(),
    );

    thread::sleep(Duration::from_millis(100));
    let stream_up = start.elapsed();
    println!("Streams up in {stream_up:?}");
    println!("Running for {run_secs} s (Ctrl-C to stop early)...\n");

    let mut next_burst = Instant::now() + Duration::from_millis(BURST_PERIOD_MS);
    let mut last_tick = Instant::now();
    let deadline = start + Duration::from_secs(run_secs);
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let now = Instant::now();
        if now >= next_burst {
            tone_state
                .lock()
                .unwrap()
                .trigger_burst(SAMPLE_RATE as f32, TONE_DUR_MS);
            *burst_emit_at.lock().unwrap() = Some(now);
            next_burst = now + Duration::from_millis(BURST_PERIOD_MS);
        }
        if now.duration_since(last_tick) >= Duration::from_secs(30) {
            let elapsed = start.elapsed().as_secs();
            let detections = rtt_samples.lock().unwrap().len();
            println!("[{elapsed:>4}s] detections so far: {detections}");
            last_tick = now;
        }
        thread::sleep(Duration::from_millis(2));
    }

    stop.store(true, Ordering::SeqCst);
    match playback_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("playback thread error: {e:?}"),
        Err(p) => eprintln!("playback thread panic: {p:?}"),
    }
    match capture_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("capture thread error: {e:?}"),
        Err(p) => eprintln!("capture thread panic: {p:?}"),
    }

    print_stats(
        stream_up,
        &in_loop_ts.lock().unwrap(),
        &out_loop_ts.lock().unwrap(),
        &rtt_samples.lock().unwrap(),
    );

    Ok(())
}

fn spawn_playback(
    device: String,
    stop: Arc<AtomicBool>,
    tone_state: Arc<Mutex<ToneState>>,
    out_loop_ts: Arc<Mutex<Vec<Instant>>>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let pcm = open_pcm(&device, Direction::Playback, 2)
            .context("open UAA for playback")?;
        let channels = pcm.hw_params_current()?.get_channels()? as usize;
        let rate = pcm.hw_params_current()?.get_rate()?;
        println!("Playback: {rate} Hz, {channels} ch, hw:UAA2,0");

        let io = pcm.io_i16()?;
        pcm.prepare()?;
        let mut buf = vec![0i16; PERIOD_FRAMES as usize * channels];

        while !stop.load(Ordering::SeqCst) {
            out_loop_ts.lock().unwrap().push(Instant::now());
            {
                let mut ts = tone_state.lock().unwrap();
                for frame in 0..PERIOD_FRAMES as usize {
                    let sf = ts.next_sample(rate as f32);
                    let s = (sf * 16384.0) as i16; // half-scale, avoid clipping
                    for ch in 0..channels {
                        buf[frame * channels + ch] = s;
                    }
                }
            }
            if let Err(e) = io.writei(&buf) {
                eprintln!("playback xrun: {e}");
                pcm.try_recover(e, true)?;
            }
        }
        Ok(())
    })
}

fn spawn_capture(
    device: String,
    stop: Arc<AtomicBool>,
    burst_emit_at: Arc<Mutex<Option<Instant>>>,
    rtt_samples: Arc<Mutex<Vec<Duration>>>,
    in_loop_ts: Arc<Mutex<Vec<Instant>>>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let pcm = open_pcm(&device, Direction::Capture, 1)
            .context("open UAA for capture")?;
        let channels = pcm.hw_params_current()?.get_channels()? as usize;
        let rate = pcm.hw_params_current()?.get_rate()?;
        println!("Capture:  {rate} Hz, {channels} ch, hw:UAA2,0");

        let io = pcm.io_i16()?;
        pcm.prepare()?;
        pcm.start()?;
        let mut buf = vec![0i16; PERIOD_FRAMES as usize * channels];

        while !stop.load(Ordering::SeqCst) {
            match io.readi(&mut buf) {
                Ok(_) => {
                    let now = Instant::now();
                    in_loop_ts.lock().unwrap().push(now);
                    let rms = compute_rms_mono_i16(&buf, channels);
                    if rms > DETECT_RMS_THRESHOLD {
                        let mut g = burst_emit_at.lock().unwrap();
                        if let Some(emit) = *g {
                            let rtt = now.duration_since(emit);
                            if rtt < MAX_PLAUSIBLE_RTT {
                                rtt_samples.lock().unwrap().push(rtt);
                                *g = None; // debounce one-detect-per-emit
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("capture xrun: {e}");
                    pcm.try_recover(e, true)?;
                }
            }
        }
        Ok(())
    })
}

fn open_pcm(device: &str, dir: Direction, desired_channels: u32) -> Result<PCM> {
    let pcm = PCM::new(device, dir, false)
        .with_context(|| format!("snd_pcm_open({device}, {dir:?})"))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_rate(SAMPLE_RATE, ValueOr::Nearest)?;
        if hwp.set_channels(desired_channels).is_err() {
            hwp.set_channels_near(desired_channels)?;
        }
        hwp.set_period_size_near(PERIOD_FRAMES as i64, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(BUFFER_FRAMES as i64)?;
        pcm.hw_params(&hwp)?;
    }
    Ok(pcm)
}

#[derive(Default)]
struct ToneState {
    remaining_samples: u32,
    phase: f32,
}

impl ToneState {
    fn trigger_burst(&mut self, sr: f32, dur_ms: u64) {
        self.remaining_samples = ((sr * dur_ms as f32) / 1000.0) as u32;
        self.phase = 0.0;
    }
    fn next_sample(&mut self, sr: f32) -> f32 {
        if self.remaining_samples == 0 {
            return 0.0;
        }
        let two_pi = std::f32::consts::TAU;
        let step = two_pi * TONE_HZ / sr;
        self.phase += step;
        if self.phase > two_pi {
            self.phase -= two_pi;
        }
        self.remaining_samples -= 1;
        self.phase.sin() * 0.5
    }
}

fn compute_rms_mono_i16(data: &[i16], channels: usize) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let scale = 1.0f32 / 32768.0;
    if channels <= 1 {
        let sum: f32 = data.iter().map(|&x| {
            let f = x as f32 * scale;
            f * f
        }).sum();
        return (sum / data.len() as f32).sqrt();
    }
    let frames = data.len() / channels;
    if frames == 0 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for i in 0..frames {
        let mut mix = 0.0f32;
        for ch in 0..channels {
            mix += data[i * channels + ch] as f32 * scale;
        }
        let avg = mix / channels as f32;
        sum += avg * avg;
    }
    (sum / frames as f32).sqrt()
}

fn print_stats(
    stream_up: Duration,
    in_ts: &[Instant],
    out_ts: &[Instant],
    rtt: &[Duration],
) {
    println!("\n========== RESULTS ==========");
    println!("Stream-start latency: {stream_up:?}");

    print_interval_stats("Capture loop", in_ts);
    print_interval_stats("Playback loop", out_ts);

    if rtt.is_empty() {
        println!("\nRound-trip: NO TONES DETECTED");
        println!("  Likely cause: no physical loopback on the UAA handset connector.");
        println!("  Jitter-only measurement complete. Loop the UAA and re-run for RTT.");
        return;
    }

    let mut micros: Vec<u64> = rtt.iter().map(|d| d.as_micros() as u64).collect();
    micros.sort_unstable();
    let p50 = percentile(&micros, 50);
    let p95 = percentile(&micros, 95);
    let p99 = percentile(&micros, 99);
    let max = *micros.last().unwrap_or(&0);
    println!("\nRound-trip latency ({} detections):", rtt.len());
    println!("  min: {}", fmt_us(micros[0]));
    println!("  p50: {}", fmt_us(p50));
    println!("  p95: {}", fmt_us(p95));
    println!("  p99: {}", fmt_us(p99));
    println!("  max: {}", fmt_us(max));
    println!(
        "  Gate (p99 < 80 ms): {}",
        if p99 < 80_000 { "PASS" } else { "FAIL" }
    );
}

fn print_interval_stats(label: &str, ts: &[Instant]) {
    if ts.len() < 2 {
        println!("\n{label}: too few samples ({})", ts.len());
        return;
    }
    let mut intervals: Vec<u64> = ts
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_micros() as u64)
        .collect();
    intervals.sort_unstable();
    let p50 = percentile(&intervals, 50);
    let p95 = percentile(&intervals, 95);
    let p99 = percentile(&intervals, 99);
    let max = *intervals.last().unwrap_or(&0);
    println!("\n{label}: {} iterations", ts.len());
    println!("  Interval p50: {}", fmt_us(p50));
    println!("  Interval p95: {}", fmt_us(p95));
    println!("  Interval p99: {}", fmt_us(p99));
    println!("  Interval max: {}", fmt_us(max));
}

fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() as u64 * p / 100) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn fmt_us(us: u64) -> String {
    if us >= 1000 {
        format!("{us} µs ({:.2} ms)", us as f64 / 1000.0)
    } else {
        format!("{us} µs")
    }
}
