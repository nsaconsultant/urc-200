# HammerHead

A browser-based remote-control head for hardware radios — starting with the General Dynamics URC-200 (V2) LOS transceiver, built to grow to the rest of the H-250 handset family (PRC-117 up next).

The radio lives in the rack with its RS-232 cable and the GD USB Audio Adapter (UAA) plugged into a small Linux box. From any device on your Tailscale network — laptop, phone, tablet — you open a web page and operate the radio as if you were standing in front of it. Tune, work presets, scan a channel library, hear what it's hearing, key up (when you unlock PTT). Optionally plug in an RSPdx / other SoapySDR device and a live waterfall opens in its own window for a second monitor.

**Status:** end-to-end functional on one operator's real hardware. Protocol, serial dispatcher, audio I/O (both directions), PTT, CTCSS tone encoder, channel library, library-scanner, manual tuning dial, server-side DSP, and — with the `sdr` feature enabled — the RSPdx-R2 waterfall are all working. Ship-to-friends grade, not ship-to-the-world grade — see [Caveats](#caveats).

---

## What it does

- **Live view of the radio** — active channel, frequency pair, mode (AM/FM), RSSI meter, squelch open/closed, synth lock, overtemp, installed hardware options. Polled at 2-5 Hz, streamed to the browser over WebSocket.
- **Change the channel** — click a P0-P9 preset, or type any RX + TX frequency in MHz. Simplex helper for one-click mirror. Band-aware validation; frequencies outside the URC-200's tuning range are rejected at the API edge.
- **Manual tuning dial** — seven-segment-style LCD on the main card that tunes with scroll wheel, touch-drag, or keyboard arrows. Shift = ×10, Ctrl = ÷10. Same dial drives both preset and arbitrary tuning paths.
- **Save a preset from the browser** — tune to what you want, click "Save to preset", pick a slot. The server orchestrates select-slot → re-apply-tune → Q (EEPROM write) automatically.
- **Channel library** — groups of channels (Aviation, Marine VHF, SATCOM, whatever). Collapsible per-group cards in the UI with delete. SQLite-backed. CSV importer auto-detects three schemas:
  - FLTSAT band-plan style (`Downlink`, `Uplink`, `Name`)
  - Chirp/ham-radio export (`Receive Frequency`, `Transmit Frequency`, `Operating Mode`, `Name`, `Step`, `CTCSS`)
  - Canonical (`name`, `rx_mhz`, `tx_mhz`, optional `mode`, `ctcss_hz`, `notes`)
  Channels outside the connected radio's supported bands still import, they're just flagged OOB in the UI and the Tune button is disabled.
- **Library scanner** — pick a group, pick dwell + settle times, stop-on-hit or continuous. Subscribes to the poller's squelch telemetry rather than re-polling, so no extra load on the serial link. Never touches TX — only `SetRx` / `SetTx` / `ModTxRx`. Auto-skips OOB channels.
- **Normal radio knobs** — lamp brightness, internal speaker, modulation AM/FM, TX power, squelch 0-255. Each maps to the corresponding Table 11 command.
- **Hear the radio** — UAA capture streams mono S16LE at 48 kHz to your browser over WebSocket. Separate controls for your computer's speaker (browser mute/volume) and the radio's internal speaker (J0/J1 on the radio itself).
- **Push-to-talk** — tap-and-hold on the on-screen pad or spacebar. Default-locked toggle prevents accidental keying. When armed, your browser mic is captured via AudioWorklet, streamed server-side, and written to the UAA playback PCM (radio's mic input). Heartbeat watchdog unkeys within 400 ms of any network or browser dropout. RX playback is auto-muted on the keying client to suppress the UAA's internal sidetone feedback loop.
- **CTCSS tone encoder in software** — the URC-200's base-band firmware doesn't encode CTCSS. Since the software owns the mic audio path, it mixes a selectable tone (67.0 - 254.1 Hz, all 50 EIA standards) into the outgoing mono stream. Tone amplitude is adjustable. Applied continuously for the duration of PTT, same as an inline hardware encoder.
- **Server-side DSP** — biquad HPF / noise gate / LPF on both RX (cleans UAA output before shipping to browser) and TX (cleans mic audio before writing to UAA, *before* CTCSS mix so the sub-audible tone isn't high-passed away). All live-tunable from sliders in the UI.
- **RSPdx / SoapySDR waterfall** (optional) — when the server is built with the `sdr` feature, a standalone `/waterfall.html` page opens a live spectrum + waterfall from any SoapySDR-compatible device. Always-visible MHz ruler with auto-nice tick spacing. Display-side zoom (1×–32×) lets you drill in on a narrow slice without changing the SDR sample rate. Plain click on the spectrum tunes the **URC-200** to that frequency (simplex, 5 kHz snap) via the existing `/api/command/tune` path; a green marker tracks where the radio is actually tuned via the telemetry WS. Shift-click (or the `SDR ⟲` / `SDR → radio` buttons) recenters the SDR viewer independently, so you can pan around without moving the demodulator. Designed to pop out in its own window on a second monitor next to the main UI. Off by default so the default build needs no SDR toolchain.
- **Single-operator lock** — multiple browsers can observe telemetry and hear RX audio; only one holds PTT at a time. First press wins; second client sees an "in use by …" state.

## Hardware

- **URC-200 (V2) LOS transceiver** — tested on a base-band unit (VHF 115-174 + UHF 225-400, no EBN-30 / EBN-400 / ECS-8 options). The protocol code handles all options; only the band-validation gate differs per radio.
- **General Dynamics USB Audio Adapter (UAA)** — provides the RX/TX audio path. Appears as a USB audio class device (`hw:UAA2,0`, VID:PID `1a16:3155`). [Teardown.](https://www.appliedcarbon.org/gdusbaudio.html)
- **USB-to-RS232 adapter** — any standard one works. Tested with a Prolific PL2303. RS-232 needs only three wires to the URC-200's J2 remote connector: pin `S` (radio TX out), pin `a` (radio RX in), pin `X` (ground). See §4.6.1 of the URC-200 manual for details.
- **SDRplay RSPdx-R2** (optional) — drives the `/waterfall.html` page. Any SoapySDR-compatible device should work; set `URC_SDR_DEVICE` to the matching device-args string. See [Waterfall setup](#waterfall-setup-optional).
- **Linux host** — DragonOS, Debian, Ubuntu all fine. Raspberry Pi 4/5 is the intended deployment target, though x86_64 is where it's been exercised.

## Protocol note for anyone else tackling the URC-200

The URC-200 manual writes acknowledgement codes as `ACK`, `NAK`, `HT`. **Those are the ASCII names of single control bytes, not 3-letter strings.** Actual wire values:

| Name | Byte |
|------|------|
| ACK  | 0x06 |
| NAK  | 0x15 |
| HT   | 0x09 |

Caught me for about thirty seconds on the first HIL query; pay it forward.

## Quick start

Requires Docker + Docker Compose v2. On Linux.

```bash
git clone https://github.com/nsaconsultant/hammerhead.git
cd hammerhead
docker compose up -d
```

The container:
- Opens `/dev/ttyUSB0` at 1200 bps 8-N-1 for the URC-200 RS-232 link. Override the path via `URC_PORT` env var in `docker-compose.yml`.
- Opens `hw:UAA2,0` for the UAA audio I/O. Override via `URC_AUDIO`.
- Serves the UI on `http://0.0.0.0:3000/`.
- Persists the channel library SQLite DB to a named volume (`urc200-data`).

On USB re-enumeration the kernel can move the PL2303 between `/dev/ttyUSB0`, `/dev/ttyUSB1`, etc. A stable udev symlink avoids this:

```bash
echo 'SUBSYSTEM=="tty", ATTRS{idVendor}=="067b", ATTRS{idProduct}=="2303", SYMLINK+="urc200-serial"' \
  | sudo tee /etc/udev/rules.d/99-urc200.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then set `URC_PORT=/dev/urc200-serial` and the PL2303 can enumerate as anything without breaking things.

## Architecture

```
┌──────────────── browser (any device on Tailnet) ────────────────┐
│   HTML/CSS/JS  ← /api/ws/telemetry (RSSI, state, preset poll)   │
│      + audio   → /api/ws/audio/rx  (UAA capture → AudioBuffer)  │
│                → /api/ws/audio/tx  (mic via AudioWorklet)       │
│                → /api/ws/control   (PTT start/heartbeat/stop)   │
│                → /api/command/*    (lamp, preset, tune, ...)    │
│                → /api/channels/*   (library CRUD, CSV import)   │
└──────────────────────────────┬──────────────────────────────────┘
                               │
                    ┌──────────▼───────────┐
                    │  urc200-server       │ Axum + WebSocket + SQLite
                    │  ─────────────────   │
                    │  PTT arbiter         │ single-owner lock, 400ms watchdog
                    │  AudioCapture (alsa) │ UAA → Arc<Vec<i16>> broadcast
                    │  AudioTx      (alsa) │ browser mic → UAA playback
                    │  CTCSS mixer         │ software tone, 67-254 Hz
                    │  Biquad HPF/LPF/gate │ server-side DSP, live-tunable
                    │  Radio (dispatcher)  │ §4.6.3 ACK/NAK/HT, 3-NAK fault
                    │  Poller              │ ?01/?03/?10/?11/?12/?13 cadence
                    │  Db (rusqlite)       │ channels + groups + migrations
                    └──────────┬───────────┘
                               │ RS-232 1200 bps 8-N-1 over PL2303
                    ┌──────────▼───────────┐     ┌────────────┐
                    │    URC-200 (V2)      │◄────┤ UAA (USB)  │ audio in/out
                    └──────────────────────┘     └────────────┘
```

Workspace crates:

- `radio-core` — the radio-agnostic trait set (`Radio`, `Mode`, `Capabilities`) that the multi-driver roadmap is being built against. Used by `urc200-serial` today; the PRC-117 driver will come next.
- `urc200-proto` — zero-I/O protocol codec (Table 11 commands, Table 13 inquiries, ACK/NAK/HT parser, band-aware Freq type). Pure logic. Heavy unit tests.
- `urc200-serial` — async `Transport` trait + `SerialTransport` (tokio-serial) + `MockTransport` + `Radio` dispatcher + `Poller`, plus `impl radio_core::Radio for Urc200Radio`.
- `urc200-server` — Axum HTTP/WebSocket host, SQLite channel library, audio I/O via ALSA, PTT arbiter, CTCSS mixer, DSP chain, library scanner, feature-gated SDR routes.
- `radio-probe` — CLI diagnostic: send one command, hex-dump the response. Hard-refuses `B` / `*1` / `I` / `K`.
- `latency-spike` — standalone tool for measuring the UAA's USB audio round-trip latency. Kept in the repo as a diagnostic.

Crates that are intentionally **not** default workspace members (so `cargo build` stays green on hosts without their toolchains):

- `radio-sdr` — SoapySDR capture + FFT waterfall feed. Pulled in by `urc200-server` only when built with `--features sdr`. See [Waterfall setup](#waterfall-setup-optional).

## API surface

```
GET  /api/health
GET  /api/features              { sdr: bool } — lets the UI show/hide feature-gated affordances
GET  /api/ws/telemetry          typed JSON events (RSSI, squelch, mode, preset, general, synth_lock)
GET  /api/ws/control            PTT protocol (hello, ptt_start, ptt_heartbeat, ptt_stop)
GET  /api/ws/audio/rx           S16LE 48 kHz mono, binary frames + JSON header
GET  /api/ws/audio/tx           mic upload, same format
GET  /api/ws/scan               library-scanner events + start/stop/skip client msgs
POST /api/command/preset/:n     0-9
POST /api/command/lamp/:l       off|lo|med|hi
POST /api/command/speaker/:s    on|off
POST /api/command/squelch/:v    0..=255
POST /api/command/mod/:m        am|fm          (both TX and RX)
POST /api/command/mod_tx_only/:m am|fm
POST /api/command/text/:t       pt|ct
POST /api/command/power/:p      lo|med|hi
POST /api/command/scan/:s       on|off
POST /api/command/scan_list_member/:s on|off
POST /api/command/store         Q — EEPROM save
POST /api/command/tune          { rx_hz, tx_hz, mode?, step? }
GET  /api/channels              list (?group=X filter)
GET  /api/channels/groups       list groups + counts
POST /api/channels/import       upload CSV text body, ?group=Name
POST /api/channels/:id/tune     apply channel + auto-arm CTCSS
DEL  /api/channels/:id
DEL  /api/channels/groups/:name
GET/POST /api/tx/ctcss          { freq_hz, amplitude? }
GET/POST /api/audio/filters/rx  { hp_enabled, hp_fc, lp_enabled, lp_fc, gate_enabled, gate_db }
GET/POST /api/audio/filters/tx  same fields

# --- only present when the server was built with `--features sdr` ---
GET  /waterfall.html            standalone second-monitor waterfall page
GET  /api/ws/waterfall          binary spectrum feed. Each frame: 16-byte LE header
                                (u64 center_hz, u32 sample_rate, u32 n_bins) + n_bins u8.
                                Client may send {"center_hz": u64} or {"gain_db": f64}
                                / {"gain_mode": "auto"} text messages to retune live.
GET/POST /api/sdr/config        { center_hz, gain_db?, gain_mode? }
```

Commands the server **deliberately does not expose**: `B` (transmit), `*1` (beacon), `I` (channel init), `K` (self-cal). Only the PTT arbiter reaches `B`, and only in response to an explicit browser gesture.

## Waterfall setup (optional)

The waterfall is an **opt-in Cargo feature**. The default build has no SDR toolchain requirements and will compile cleanly anywhere Rust + ALSA headers live. Only turn it on if you have the host libraries and actually want the panadapter.

### Host prerequisites (Debian / Ubuntu / DragonOS)

Build-time (only when compiling `--features sdr`):

```bash
sudo apt install clang libclang-dev libsoapysdr-dev pkg-config
```

Runtime:

```bash
sudo apt install libsoapysdr0.8
# plus the vendor SDR runtime — e.g. SDRplay API 3.15 from sdrplay.com
# for an RSPdx / RSPdx-R2. Installs sdrplay_apiService + the Soapy
# SDRplay bridge module and starts the daemon as a systemd unit.
```

The SDRplay API ships a userspace daemon (`sdrplay_apiService`) that must be running and own the USB device. Our in-container build talks to it via POSIX shared memory + semaphores — no TCP involved.

`libstdc++` requirement: the SoapySDR SDRplay module ships with `GLIBCXX_3.4.32` symbols, so whichever libstdc++ the process loads must be from GCC 13+ (Debian trixie / Ubuntu 24.04 / DragonOS). Bookworm's GCC 12 is not enough; the compose file bind-mounts the host's libstdc++ into the container for exactly this reason.

### Native build

```bash
cargo build --release -p urc200-server --features sdr
```

If the build fails with `fatal error: 'stdbool.h' file not found`, clang can't find its resource headers. Either install the `clang` binary (provides them) or point bindgen at gcc's:

```bash
BINDGEN_EXTRA_CLANG_ARGS='-I/usr/lib/gcc/x86_64-linux-gnu/13/include' \
  cargo build --release -p urc200-server --features sdr
```

### Docker build (recommended)

`docker-compose.yml` is already wired for the SDR path. `sudo docker compose up -d --build` is all you need, provided the host prerequisites above are installed. Key non-obvious bits the compose file sets up (each commented inline — spelling them out here so it's clear what's going on):

- `network_mode: host` — lets the container reach the host-side SDRplay daemon without port gymnastics. Also means `:3000` is exposed directly with no `ports:` mapping.
- `ipc: host` — shares `/dev/shm` with the host. The SDRplay API uses POSIX shm + semaphores for client↔daemon IPC; private containers can't see those.
- `pid: host` — the SDRplay client writes its own PID into a shm slot so the daemon can track live clients. In an isolated PID namespace that PID doesn't exist on the host and the daemon rejects the registration.
- **Bind-mounts** from host into container:
  - `/usr/local/lib/SoapySDR` — the SoapySDR module directory, including `libsdrPlaySupport.so`
  - `/usr/local/lib/libsdrplay_api.so*` — the vendor client API
  - `/opt/sdrplay_api` — daemon install dir
  - `/usr/lib/x86_64-linux-gnu/libstdc++.so.6` — newer libstdc++ needed by the SDRplay module (see above)
- `devices:` — `/dev/ttyUSB0` for the URC-200 serial link, `/dev/snd` for the UAA audio path (unchanged from the non-SDR build).

### Runtime config (env)

| var              | default            | notes                                                     |
|------------------|--------------------|-----------------------------------------------------------|
| `URC_SDR_DEVICE` | `driver=sdrplay`   | SoapySDR device-args string. e.g. `driver=rtlsdr`, `driver=sdrplay,serial=24051CD970` |
| `URC_SDR_CENTER` | `251950000`        | Start-up center frequency in Hz. Retuned live from the UI. |
| `SOAPY_SDR_LOG_LEVEL` | *(unset)*     | Set to `DEBUG` to see module-load + enumeration errors.    |

**Using it:** with the server running, the main UI header shows a `Waterfall ↗` link (hidden when the feature isn't compiled in). Click it — it opens `/waterfall.html` as a named window target, so it lands on whichever monitor you last dragged it to. Controls:

| action                             | effect                                                             |
|------------------------------------|--------------------------------------------------------------------|
| plain click on spectrum/waterfall  | tune the URC-200 to that frequency (RX = TX, 5 kHz grid)           |
| shift-click on spectrum/waterfall  | recenter the SDR window on that frequency (radio untouched)        |
| type MHz + **Tune radio**          | tune the URC-200                                                   |
| type MHz + **SDR ⟲**               | recenter the SDR window only                                       |
| **SDR → radio** button             | recenter the SDR on the URC-200's current RX frequency             |
| **zoom** dropdown (1× … 32×)       | display-side zoom — slices the FFT we already have, no SDR churn   |
| **gain** dropdown                  | auto AGC or fixed 30/40/50/60 dB                                   |
| hover                              | cursor line + live MHz readout                                     |

The green ◆ on the spectrum and its label track where the URC-200 is actually tuned (subscribed to the same `/api/ws/telemetry` the main UI uses).

## Caveats

- **FCC / licensed operation only.** The URC-200 is a serious transmitter. Anything you do with it is your license on the line, same as any rig. This software is a front panel, not a guarantee of compliance.
- **No in-app auth.** Access model is Tailscale-only — the Tailscale ACL is the auth. Do not put this on the open internet.
- **Tested on one unit.** Base-band URC-200 (V2) with no EBN-30 / EBN-400 / ECS-8 options. Radios with those options should work (the protocol code is option-aware) but haven't been exercised.
- **TX audio feedback suppression is client-side.** While a browser holds PTT, its own RX playback is muted to avoid the UAA's internal codec-sidetone feedback loop. Multi-operator scenarios (two browsers open, one keying) work because observers don't mute — but if your RX audio is coming out of the same speakers that feed a microphone somewhere else in the room, you'll still get feedback. Not a software problem.
- **Docker bind-mount gotcha.** Never bind `:/dev/ttyUSB0:/dev/ttyUSB0` via `volumes:` — if the source path is missing at up time, Docker creates an empty *directory* on the host, which blocks udev from creating a device node when the adapter returns. Use `devices:` instead (present in this repo's `docker-compose.yml`).

## License

Apache-2.0. See [LICENSE](LICENSE).
