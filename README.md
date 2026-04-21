# urc200-head

A browser-based remote-control head for the General Dynamics URC-200 (V2) LOS transceiver.

The radio lives in the rack with its RS-232 cable and the GD USB Audio Adapter (UAA) plugged into a small Linux box. From any device on your Tailscale network — laptop, phone, tablet — you open a web page and operate the radio as if you were standing in front of it. Tune, work presets, hear what it's hearing, key up (when you unlock PTT). The chunky rack radio becomes something you can drive from the couch.

**Status:** end-to-end functional on one operator's real hardware. Protocol, serial dispatcher, audio I/O (both directions), PTT, CTCSS tone encoder, and channel library are all working. Ship-to-friends grade, not ship-to-the-world grade — see [Caveats](#caveats).

---

## What it does

- **Live view of the radio** — active channel, frequency pair, mode (AM/FM), RSSI meter, squelch open/closed, synth lock, overtemp, installed hardware options. Polled at 2-5 Hz, streamed to the browser over WebSocket.
- **Change the channel** — click a P0-P9 preset, or type any RX + TX frequency in MHz. Simplex helper for one-click mirror. Band-aware validation; frequencies outside the URC-200's tuning range are rejected at the API edge.
- **Save a preset from the browser** — tune to what you want, click "Save to preset", pick a slot. The server orchestrates select-slot → re-apply-tune → Q (EEPROM write) automatically.
- **Channel library** — groups of channels (Aviation, Marine VHF, SATCOM, whatever). SQLite-backed. CSV importer auto-detects three schemas:
  - FLTSAT band-plan style (`Downlink`, `Uplink`, `Name`)
  - Chirp/ham-radio export (`Receive Frequency`, `Transmit Frequency`, `Operating Mode`, `Name`, `Step`, `CTCSS`)
  - Canonical (`name`, `rx_mhz`, `tx_mhz`, optional `mode`, `ctcss_hz`, `notes`)
  Channels outside the connected radio's supported bands still import, they're just flagged OOB in the UI and the Tune button is disabled.
- **Normal radio knobs** — lamp brightness, internal speaker, modulation AM/FM, TX power, squelch 0-255. Each maps to the corresponding Table 11 command.
- **Hear the radio** — UAA capture streams mono S16LE at 48 kHz to your browser over WebSocket. Separate controls for your computer's speaker (browser mute/volume) and the radio's internal speaker (J0/J1 on the radio itself).
- **Push-to-talk** — tap-and-hold on the on-screen pad or spacebar. Default-locked toggle prevents accidental keying. When armed, your browser mic is captured via AudioWorklet, streamed server-side, and written to the UAA playback PCM (radio's mic input). Heartbeat watchdog unkeys within 400 ms of any network or browser dropout.
- **CTCSS tone encoder in software** — the URC-200's base-band firmware doesn't encode CTCSS. Since the software owns the mic audio path, it mixes a selectable tone (67.0 - 254.1 Hz, all 50 EIA standards) into the outgoing mono stream. Tone amplitude is adjustable. Applied continuously for the duration of PTT, same as an inline hardware encoder.
- **Server-side DSP** — biquad HPF / noise gate / LPF on both RX (cleans UAA output before shipping to browser) and TX (cleans mic audio before writing to UAA, *before* CTCSS mix so the sub-audible tone isn't high-passed away). All live-tunable.
- **Single-operator lock** — multiple browsers can observe telemetry and hear RX audio; only one holds PTT at a time. First press wins; second client sees an "in use by …" state.

## Hardware

- **URC-200 (V2) LOS transceiver** — tested on a base-band unit (VHF 115-174 + UHF 225-400, no EBN-30 / EBN-400 / ECS-8 options). The protocol code handles all options; only the band-validation gate differs per radio.
- **General Dynamics USB Audio Adapter (UAA)** — provides the RX/TX audio path. Appears as a USB audio class device (`hw:UAA2,0`, VID:PID `1a16:3155`). [Teardown.](https://www.appliedcarbon.org/gdusbaudio.html)
- **USB-to-RS232 adapter** — any standard one works. Tested with a Prolific PL2303. RS-232 needs only three wires to the URC-200's J2 remote connector: pin `S` (radio TX out), pin `a` (radio RX in), pin `X` (ground). See §4.6.1 of the URC-200 manual for details.
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
git clone https://github.com/YOUR-USER/urc200-head.git
cd urc200-head
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

- `urc200-proto` — zero-I/O protocol codec (Table 11 commands, Table 13 inquiries, ACK/NAK/HT parser, band-aware Freq type). Pure logic. Heavy unit tests.
- `urc200-serial` — async `Transport` trait + `SerialTransport` (tokio-serial) + `MockTransport` + `Radio` dispatcher + `Poller`.
- `urc200-server` — Axum HTTP/WebSocket host, SQLite channel library, audio I/O via ALSA, PTT arbiter, CTCSS mixer, DSP chain.
- `radio-probe` — CLI diagnostic: send one command, hex-dump the response. Hard-refuses `B` / `*1` / `I` / `K`.
- `latency-spike` — standalone tool for measuring the UAA's USB audio round-trip latency. Kept in the repo as a diagnostic.

## API surface

```
GET  /api/health
GET  /api/ws/telemetry          typed JSON events (RSSI, squelch, mode, preset, general, synth_lock)
GET  /api/ws/control            PTT protocol (hello, ptt_start, ptt_heartbeat, ptt_stop)
GET  /api/ws/audio/rx           S16LE 48 kHz mono, binary frames + JSON header
GET  /api/ws/audio/tx           mic upload, same format
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
```

Commands the server **deliberately does not expose**: `B` (transmit), `*1` (beacon), `I` (channel init), `K` (self-cal). Only the PTT arbiter reaches `B`, and only in response to an explicit browser gesture.

## Caveats

- **FCC / licensed operation only.** The URC-200 is a serious transmitter. Anything you do with it is your license on the line, same as any rig. This software is a front panel, not a guarantee of compliance.
- **No in-app auth.** Access model is Tailscale-only — the Tailscale ACL is the auth. Do not put this on the open internet.
- **Tested on one unit.** Base-band URC-200 (V2) with no EBN-30 / EBN-400 / ECS-8 options. Radios with those options should work (the protocol code is option-aware) but haven't been exercised.
- **TX audio feedback suppression is client-side.** While a browser holds PTT, its own RX playback is muted to avoid the UAA's internal codec-sidetone feedback loop. Multi-operator scenarios (two browsers open, one keying) work because observers don't mute — but if your RX audio is coming out of the same speakers that feed a microphone somewhere else in the room, you'll still get feedback. Not a software problem.
- **Docker bind-mount gotcha.** Never bind `:/dev/ttyUSB0:/dev/ttyUSB0` via `volumes:` — if the source path is missing at up time, Docker creates an empty *directory* on the host, which blocks udev from creating a device node when the adapter returns. Use `devices:` instead (present in this repo's `docker-compose.yml`).

## License

Apache-2.0. See [LICENSE](LICENSE).
