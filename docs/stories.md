# urc200-head — User Stories

Source of truth: party-mode roundtable decisions (Winston's lock).
Audience personas:
- **Operator** — Hammer + a few URC-owning friends, operating the radio remotely from a PC.
- **Maintainer** — same people wearing a different hat: diagnosing, adjusting, importing preset packs.
- **Developer** — internal; Hammer building/testing the tool.

MVP = Epics 1–7. Everything else is parked.

---

## Epic 1 — Spike & Foundation

### S-001: Measure UAA round-trip latency
**As a** developer
**I want** a standalone Rust binary that opens the UAA for simultaneous capture + playback and logs round-trip latency stats over 10 minutes
**So that** we know whether the 250 ms PTT watchdog budget is realistic before building anything else

**Acceptance Criteria**
- [ ] Binary opens `UAA_2` (ALSA card 1, VID:PID 1a16:3155) for input AND output at 48 kHz f32 mono
- [ ] Emits a 1 kHz tone burst every 500 ms out the UAA output
- [ ] Captures UAA input, correlates tone onset, computes round-trip latency per burst
- [ ] After 10 min: prints min / p50 / p95 / p99 / max, callback jitter, XRUN count
- [ ] Pass criterion: **p99 < 80 ms**. Fail: re-examine watchdog budget + interaction model.

**Notes**
- Physical loopback cable needed: UAA audio-out → UAA audio-in (H-250 breakout or test harness). If no cable, Phase-0 sub-spike just measures cpal callback jitter without the RF/audio loop.
- No radio attached for this spike.
- Deliverable lives at `crates/latency-spike/` — thrown away or kept as a diagnostic tool.

---

### S-002: Install Rust toolchain + scaffold workspace
**As a** developer
**I want** `rustup` installed and a Cargo workspace matching Amelia's layout
**So that** subsequent stories have a landing place

**Acceptance Criteria**
- [ ] `rustup` installed; `rustc 1.84+` on PATH
- [ ] Workspace root `/home/sdr/urc200-head/Cargo.toml` with members: `crates/urc200-proto`, `crates/urc200-serial`, `crates/uaa-audio`, `crates/urc200-hil`, `crates/latency-spike`, `src-tauri`
- [ ] Empty `src/` placeholder for SvelteKit frontend
- [ ] `cargo check --workspace` green

**Notes**
- `rustup default stable`
- Tauri CLI: `cargo install tauri-cli --version "^2.1"` — deferred until Epic 7.

---

### S-003: Detect & bind the UAA audio device reliably
**As a** developer
**I want** a `uaa_detect` module that finds the UAA by USB VID:PID, returns its ALSA card index and cpal `Device` handle
**So that** the rest of the app never has to guess

**Acceptance Criteria**
- [ ] `rusb` enumerates VID `0x1a16` PID `0x3155` and returns found/not-found
- [ ] `cpal` device enumeration matches against the UAA by substring "UAA_2" with USB-path fallback
- [ ] Returns structured result: `{ capture: Device, playback: Device, alsa_card: u8 }`
- [ ] Unit test: `--mock` mode returns a synthetic device for CI

---

## Epic 2 — URC-200 Protocol (`urc200-proto`)

### S-010: Encode Table 11 operation commands
**As a** developer
**I want** a typed codec that serializes every Table 11 command to ASCII bytes
**So that** the app can never emit an invalid command

**Acceptance Criteria**
- [ ] Enum `OpCommand` covering: `Z`, `Squelch(u8)`, `Init`, `Lamp(L0..L3)`, `Speaker(bool)`, `Calibrate`, `Preset(u4)`, `SetRx(Freq)`, `SetTx(Freq)`, `Mod{mode, txrx|tx_only}`, `Text(PT|CT)`, `Store`, `ScanListMember(bool)`, `PowerLevel(Lo|Med|Hi)`, `Scan(bool)`, `Beacon(bool)`, `ReleaseKeypad`, `Transmit`, `Receive`, `ToneSquelch(off|rx|tx|both)`
- [ ] `Freq` type enforces band-aware encoding (base, EBN-30 `0xxxxx`, EBN-400 `4xxxxx`, 25/12.5/5/8.33 kHz divisibility)
- [ ] Proptest round-trip: encode → decode → equal (for decodable commands)
- [ ] NAK-returning commands (CT in aviation mode, <2-channel scan) gated at type level where possible

### S-011: Decode responses + ACK/NAK/HT FSM
**As a** developer
**I want** a response decoder and a command-issue FSM
**So that** the app tracks conversation state rigorously per §4.6.3

**Acceptance Criteria**
- [ ] Parser recognizes: `ACK`, `NAK`, `HT`, data-then-ACK pairs for every `?nn` inquiry
- [ ] FSM states: `Idle → AwaitingAck{cmd, deadline} → {Acked, Naked(count), TimedOut, HtThenAck}`
- [ ] Timeout configurable (default 500 ms per command)
- [ ] 3-NAK fault surfaces a typed error
- [ ] `Z` resync issued automatically on timeout, before declaring fault

### S-012: Decode Table 12 customizing + Table 13 status inquiries
**As a** maintainer
**I want** every `e`, `q`, `^`, `!`, `<`, `W` customizing command and every `?01..?99` inquiry typed in Rust
**So that** the maintenance surface is a data model, not string parsing

**Acceptance Criteria**
- [ ] Inquiry results: `SynthLock`, `ScanDetect`, `Rssi`, `CalStatus`, `PowerRails{+5, +12, -5, -12, +24, +70}`, `SwVersion`, `SquelchLevel`, `PresetSnapshot`, `GeneralStatus{pt_ct, spkr, lamp, opts, overtemp}`, `Mode{rx, tx, beacon}`, `SquelchStatus`, `WarpValue`, `ToneStatus`, `AviationMode`, `BandwidthSel`, plus `?20–?70` deviation, `?86–?88` analog/discrete
- [ ] Unit tests with fixtures in `tests/fixtures/*.ascii` — one file per inquiry, recorded when HIL is available

### S-013: MockTransport for CI
**As a** developer
**I want** a `Transport` trait + `MockTransport` with scripted responses
**So that** the protocol crate tests without real hardware

**Acceptance Criteria**
- [ ] Trait with `async send(&mut self, bytes: &[u8]) -> Result<()>` and a `subscribe()` stream of received bytes
- [ ] Mock drives from a `VecDeque<Response>` or a `Vec<(RequestMatcher, Response)>` script
- [ ] `cargo test -p urc200-proto` runs with zero hardware

---

## Epic 3 — Serial Transport (`urc200-serial`)

### S-020: Open serial port with autodetect + reconnect
**As an** operator
**I want** the app to find the URC-200 cable automatically and recover if the cable is pulled
**So that** I don't lose operating time to stale state

**Acceptance Criteria**
- [ ] Enumerate `/dev/ttyUSB*`, `/dev/ttyACM*`, COMn via `serialport` crate
- [ ] Default config: 1200 bps, 8-N-1, no flow control
- [ ] Reconnect loop with exponential backoff (1 s → 30 s), surfaces state to UI
- [ ] User can override the auto-picked port in config TOML

### S-021: Command queue + response dispatch
**As a** developer
**I want** a single tokio task owning the serial port with a mpsc request queue and a broadcast response bus
**So that** no other task can race the master/slave protocol

**Acceptance Criteria**
- [ ] Public API: `SerialBus::send(OpCommand) -> oneshot<Result<Response>>`
- [ ] In-flight commands serialize (strict FIFO per §4.6.3 — no pipelining)
- [ ] BREAK events logged but non-fatal per Winston's ruling

### S-022: Telemetry poller
**As an** operator
**I want** the app to refresh RSSI / squelch / mode / general status at ~2 Hz without crowding out my commands
**So that** the UI feels alive

**Acceptance Criteria**
- [ ] Configurable poll cadence (default: `?03` every 500 ms, `?11` every 1 s, `?13` every 500 ms, `?10` every 2 s)
- [ ] User command pre-empts the next scheduled poll
- [ ] Poller backs off to 5 s when UI is hidden / minimized

---

## Epic 4 — Audio I/O (`uaa-audio`)

### S-030: Bind UAA with cpal, report supported formats
**As a** developer
**I want** cpal input + output streams bound to the UAA at a negotiated sample rate
**So that** we don't guess and get zero audio at runtime

**Acceptance Criteria**
- [ ] Negotiate 48 kHz f32 mono first; fall back to 16-bit int if needed; log the chosen format
- [ ] 512-frame buffer target; warn if driver forces a larger one
- [ ] Surface XRUN events to the UI

### S-031: RX path — UAA input → default output
**As an** operator
**I want** to hear the radio's receive audio on my default output device (speakers, headset)
**So that** the URC-200 is no longer the sound source in my shack

**Acceptance Criteria**
- [ ] SPSC ring buffer 4800 frames (100 ms) between UAA capture and system output
- [ ] Resample via `rubato` if default output sample rate ≠ 48 kHz
- [ ] Mute toggle in UI halts playback but leaves the capture stream live (for metering)

### S-032: TX path — system input → UAA output, gated on PTT
**As an** operator
**I want** my mic audio to reach the radio ONLY while PTT is held
**So that** I never broadcast my shack conversation

**Acceptance Criteria**
- [ ] Default system input selectable (dropdown); zero-fill samples when `PttState != Keyed`
- [ ] TX audio un-mute occurs AFTER ACK of `B` command (see S-051)
- [ ] Configurable tail delay (150–250 ms) before `E` command on release — prevents clipped word tails

---

## Epic 5 — PTT

### S-040: HID PTT from UAA handset (invisible to OS cursor)
**As an** operator
**I want** to squeeze my H-250 handset and have the radio key up without the app moving my mouse cursor or grabbing OS focus
**So that** the handset feels like the real radio's PTT

**Acceptance Criteria**
- [ ] `hidapi` opens the UAA's HID interface by VID:PID `0x1a16:0x3155` (claim = raw, not OS mouse hook)
- [ ] Press/release edge detection on the HID report byte that carries the click
- [ ] Cursor on the desktop does NOT move when the handset is squeezed
- [ ] udev rule shipped in `udev/99-urc200-uaa.rules` granting `plugdev` group `hidraw` access

### S-041: Spacebar + GUI PTT
**As an** operator
**I want** the spacebar and an on-screen button to also key the radio
**So that** I can operate from the keyboard or a trackpad

**Acceptance Criteria**
- [ ] Spacebar is hold-to-key by default; toggle mode opt-in
- [ ] GUI PTT tile responds to mouse-down / mouse-up
- [ ] Key repeat does not cause transmit hiccups (debounced in arbiter)

### S-042: Footswitch (stretch — MVP if trivial, else V0.2)
**As an** operator
**I want** to learn a footswitch via a "press now" button
**So that** I can key hands-free

**Acceptance Criteria**
- [ ] Any HID input device can be mapped via a one-button learn flow
- [ ] Binding persisted to config TOML

### S-043: PTT arbiter + unified state machine
**As a** developer
**I want** one canonical `PttState` owner, fed by all sources, with deterministic transitions
**So that** the radio's keyed state is never in dispute

**Acceptance Criteria**
- [ ] `enum PttSource { Space, Gui, Footswitch, HandsetHid }`
- [ ] `tokio::sync::watch<PttState>` as the single source of truth
- [ ] Any source pressed → Keyed. ALL sources released → Unkeying (after tail delay) → Idle.
- [ ] Debounce: 15 ms press, 50 ms release
- [ ] Unit test: property test across {race, flap, focus-loss, drop}

---

## Epic 6 — Safety (non-negotiable)

### S-050: Dead-carrier watchdog
**As the** FCC
**I want** this app to unkey the radio whenever the audio stream dies mid-transmit
**So that** Hammer does not emit a dead carrier

**Acceptance Criteria**
- [ ] cpal `StreamError` OR 500 ms without an audio callback while `Keyed` → emit `E\r` synchronously
- [ ] Integration test asserts `E\r` on a loopback serial within **250 ms** of injected `StreamError`
- [ ] Watchdog event surfaces to UI as a loud red banner

### S-051: Panic + Drop guarantee
**As the** FCC
**I want** any app crash or graceful shutdown to unkey the radio before the process exits
**So that** a panic cannot leave me on the air

**Acceptance Criteria**
- [ ] `std::panic::set_hook` flushes `E\r` before default hook runs
- [ ] `Drop` on the `RadioHandle` sends `E\r` synchronously
- [ ] Test with `std::process::abort` acceptable-to-skip; test with `panic!()` required-to-pass
- [ ] Kill -9 is NOT required to unkey (documented limitation)

### S-052: Stuck-key & max-TX guard
**As an** operator
**I want** an absolute ceiling on single-transmission duration
**So that** a stuck spacebar can't transmit for hours

**Acceptance Criteria**
- [ ] Configurable max-TX (default 30 s); exceeding it forces `E` and a UI warning
- [ ] Stuck modifier detection: if OS reports key-down for longer than configurable window, release-as-if-up

### S-053: Hot-unplug of serial or UAA
**As an** operator
**I want** cable-pull detection to unkey immediately and reconnect cleanly
**So that** a loose cable doesn't trap the radio on-air

**Acceptance Criteria**
- [ ] `tokio_serial::SerialStream` error → force `PttState = Idle`, enter reconnect loop
- [ ] UAA HID/audio disconnect → force `PttState = Idle` + emit `E\r` via serial
- [ ] Manual smoke: yank UAA mid-TX, observe unkey ≤ 1 s

### S-054: Beacon-mode confirmation
**As an** operator
**I want** a two-step confirmation before the app issues `*1` (Beacon)
**So that** I don't accidentally camp a carrier on a live frequency

**Acceptance Criteria**
- [ ] Beacon toggle opens a modal: "BEACON MODE — continuous carrier on {freq}. Continue?"
- [ ] Current RX frequency displayed; max beacon duration configurable (default 5 min, hard cap 15 min)
- [ ] Active beacon banner always visible; one-click abort

---

## Epic 7 — User Interface (`src/` + `src-tauri/`)

### S-060: Tauri shell + Svelte/Solid scaffold
**As a** developer
**I want** a Tauri 2.1 app skeleton that invokes Rust commands over IPC
**So that** the UI has a landing spot

**Acceptance Criteria**
- [ ] `cargo tauri dev` opens a window showing "URC-200 Head v0.1"
- [ ] Tauri invoke handler echoes a ping/pong
- [ ] Frontend bundler: SvelteKit static adapter

### S-061: Operator / Maintainer mode toggle
**As an** operator
**I want** a single segmented control at the top of the window to flip between "Operate" and "Maintain"
**So that** diagnostics don't clutter my operating surface

**Acceptance Criteria**
- [ ] Operate: presets, PTT, audio strip, activity waterfall, channel card, logbook pane
- [ ] Maintain: raw serial trace, BIT rail voltages, synth lock, temp, firmware version, calibration control, crystal warp
- [ ] State persists across sessions (config TOML)

### S-062: Active Channel card (LCD-font)
**As an** operator
**I want** the current frequency, preset number, and mode shown as a large LCD-style readout at the top of the screen
**So that** I can see everything at a glance

**Acceptance Criteria**
- [ ] Font: 7-segment or VCR-OSD style; ≥48 px at default zoom
- [ ] Fields: TX freq, RX freq, mode (AM/FM), text (PT/CT), preset (P0–P9), power (L/M/H), scan list flag
- [ ] Live-updates from `?10` poll

### S-063: Preset grid P0–P9
**As an** operator
**I want** ten large preset tiles I can click to arm a channel
**So that** the "talk now" flow is one click

**Acceptance Criteria**
- [ ] Grid of 10 tiles, each showing label + TX/RX pair + mode badge
- [ ] Click → sends `Px`, updates the Active Channel card
- [ ] Right-click or "Edit" button opens preset editor drawer

### S-064: Audio strip (persistent)
**As an** operator
**I want** a full-width audio strip along the bottom of the window showing RX VU, TX VU, device selectors, and a loopback test button
**So that** audio is always visible and testable

**Acceptance Criteria**
- [ ] RX VU + peak hold + CLIP latch (2 s)
- [ ] TX VU lights only while Keyed, with a sidetone LED
- [ ] Device pills (`RX: UAA_2` / `TX: UAA_2`); click opens drawer with dropdowns + gain sliders
- [ ] "Loopback test" button plays a 3 s test tone through TX → RX path

### S-065: TX banner
**As an** operator
**I want** a full-width red banner at the top of the window whenever the carrier is up
**So that** I can never accidentally stay keyed

**Acceptance Criteria**
- [ ] Visible only while `PttState == Keyed`
- [ ] Label: "ON AIR — {freq}"
- [ ] Animated pulse; colorblind-safe red (high-lum orange-red)

### S-066: Activity strip (RSSI + audio FFT)
**As an** operator
**I want** a thin waterfall showing RSSI over time AND a live audio-band FFT
**So that** I can see the channel breathing without needing an SDR

**Acceptance Criteria**
- [ ] Top lane: RSSI timeseries from `?03` @ ~2 Hz, 60 s rolling window
- [ ] Bottom lane: audio FFT (512-bin, log-freq) of the UAA RX stream
- [ ] Labeled "Activity" — not "Spectrum" — to set expectations honestly

### S-067: Field theme
**As an** operator in the field
**I want** a high-contrast amber-on-black theme with large hit targets
**So that** the app is usable on a sunlit laptop

**Acceptance Criteria**
- [ ] Toggle in settings: Default / Field / Light
- [ ] Field theme: ≥48 px hit targets, WCAG-AA contrast, compact 900×600 window mode available

### S-068: First-run config (single TOML)
**As an** operator installing the app on a fresh machine
**I want** a single `config.toml` that specifies my serial port, UAA device, and PTT VID:PID
**So that** setup is "edit one file and launch"

**Acceptance Criteria**
- [ ] File path: `~/.config/urc200-head/config.toml` (Linux) / `%APPDATA%\urc200-head\config.toml` (Windows)
- [ ] Template written on first run if missing; sensible defaults
- [ ] App refuses to start with a loud, explicit error if required fields are missing

---

## Epic 8 — Presets & Data

### S-070: SQLite schema
**As a** developer
**I want** a minimal schema for presets and session logs
**So that** data lives in one place

**Acceptance Criteria**
- [ ] Tables: `presets(p0..p9 ...)`, `sessions(id, start, end, ...)`, `session_events(session_id, ts, kind, payload)`
- [ ] Migrations via `rusqlite_migration`
- [ ] WAL mode enabled

### S-071: Preset CRUD
**As an** operator
**I want** to read, edit, and write the 10 presets with validation of band-dependent constraints
**So that** I can build my channel lineup without fighting the radio

**Acceptance Criteria**
- [ ] Edit drawer validates frequency step, band availability, and AM/FM compatibility before allowing "Save to Radio"
- [ ] "Read All" pulls all 10 from radio and diffs against local DB
- [ ] "Write All" applies local DB to radio with per-channel confirmation of ACKs

### S-072: FLTSAT CSV import
**As an** operator
**I want** to import the `~/Downloads/satcom frequencies.csv` FLTSAT band plan and select a Band Plan (A/B/C) to load into presets
**So that** satcom ops are one click away

**Acceptance Criteria**
- [ ] CSV parser handles the existing schema (detect columns: channel, band_plan, downlink, uplink, mode)
- [ ] UI: "Import CSV → pick Band Plan → preview 10-row mapping → Write to Radio"
- [ ] Preset labels auto-populate with channel name from CSV

### S-073: Session logging
**As an** operator
**I want** every PTT press, frequency change, and error to be timestamped in a session log
**So that** I can review my operating afterwards

**Acceptance Criteria**
- [ ] Events written synchronously to SQLite
- [ ] Log pane (Operate mode) shows last 200 events
- [ ] Export-to-CSV button on session list

---

## Parked (Post-MVP)

| ID | Title | Reason parked |
|---|---|---|
| P-001 | Squelch-triggered audio capture | Coupled to safety subsystem; Winston's call: after watchdog has real-use telemetry |
| P-002 | SDR-coupled tuning (RSPdx-R2) | Full second project; SoapySDR + sync FSM + waterfall UI. Use SDRuno alongside for now. |
| P-003 | Dual-VFO via preset ping-pong | EEPROM wear risk; clever-not-useful. Cut. |
| P-004 | ADIF logbook export | Nice to have; not blocking |
| P-005 | Scripting/macro engine | V2+ if ever |
| P-006 | Mobile companion | Scope trap — cut |
| P-007 | Multi-operator remote | Scope trap — cut |
