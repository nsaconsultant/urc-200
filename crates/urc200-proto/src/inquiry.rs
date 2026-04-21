//! Table 13 status inquiry commands (§4.6.4).
//!
//! `Inquiry` encodes to the `?NN` wire format (3 bytes: '?' + two digits).
//! Responses are data-bearing: per §4.6.3 the radio emits the data payload
//! first, then ACK. `ResponseParser` already lifts the payload into
//! `Response::Ack { data }`; the decoders here take that payload and produce
//! strongly-typed values.
//!
//! MVP typed decoders: `SynthLock`, `Rssi`, `PresetSnapshot`, `GeneralStatus`,
//! `Mode`, `SquelchStatus`. Everything else is encodable but caller gets raw
//! bytes until a follow-up story.

use crate::command::{LampLevel, ModMode, PowerLevel, PresetId, TextMode};

/// Every Table 13 inquiry the radio supports. Variants use the manual's ?NN
/// numbering where practical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Inquiry {
    SynthLock = 1,
    ScanDetect = 2,
    Rssi = 3,
    CalStatus = 4,
    PowerRails = 5,
    VFwd = 6,
    VRfd = 7,
    SwVersion = 8,
    SquelchLevel = 9,
    PresetSnapshot = 10,
    GeneralStatus = 11,
    Mode = 12,
    SquelchStatus = 13,
    WarpValue = 14,
    PowerTable = 16,
    TuningFilter = 17,
    Tone3090 = 19,
    AviationMode = 89,
    RxFilterBw = 90,
    Debug = 99,
}

impl Inquiry {
    /// Encode to the 3-byte wire form `?NN` where NN is zero-padded.
    pub fn encode(self) -> [u8; 3] {
        let n = self as u8;
        [b'?', b'0' + (n / 10), b'0' + (n % 10)]
    }
}

// ---------- Typed decoders (MVP subset) ----------

/// `?01` response — synth lock status. Wire: `A0` (unlocked) / `A1` (locked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynthLock(pub bool);

impl SynthLock {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        match data {
            b"A0" => Some(Self(false)),
            b"A1" => Some(Self(true)),
            _ => None,
        }
    }
}

/// `?03` response — received signal strength. Wire: `Nxxx` where xxx is 0-255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rssi(pub u8);

impl Rssi {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let digits = strip_prefix(data, b'N')?;
        if digits.len() != 3 {
            return None;
        }
        let v = parse_ascii_digits(digits)?;
        Some(Self(v.min(255) as u8))
    }
}

/// `?12` response — current operating mode. Wire: `*1`, `U0`, or `U1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Receive,
    Transmit,
    Beacon,
}

impl Mode {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        match data {
            b"U0" => Some(Mode::Receive),
            b"U1" => Some(Mode::Transmit),
            b"*1" => Some(Mode::Beacon),
            _ => None,
        }
    }
}

/// `?13` response — squelch status. Wire: `[0` (closed) / `[1` (broken).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquelchStatus {
    Closed,
    Broken,
}

impl SquelchStatus {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        match data {
            b"[0" => Some(SquelchStatus::Closed),
            b"[1" => Some(SquelchStatus::Broken),
            _ => None,
        }
    }
}

/// `?10` response — current preset snapshot.
/// Wire: concatenated fields (order per manual Table 13):
///   `Txxxxxxx` (TX freq, 7 digits = 100 Hz units)
///   `Rxxxxxxx` (RX freq, 7 digits = 100 Hz units)
///   `Mx` (AM=0, FM=1 — both TX and RX)
///   `Nx` (AM=0, FM=1 — TX only)
///   `Cx` (scan-list member 0/1)
///   `Px` (preset number 0-9)
///   `#x` (power level 0/1/2)
///
/// The parser scans field-by-field rather than assuming strict ordering, so it
/// tolerates future firmware rearrangements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetSnapshot {
    pub tx_hz: u32,
    pub rx_hz: u32,
    pub mod_tx_rx: ModMode,
    pub mod_tx_only: ModMode,
    pub on_scan_list: bool,
    pub preset: PresetId,
    pub power: PowerLevel,
}

impl PresetSnapshot {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let tx = extract_digits_after(data, b'T', 7)?;
        let rx = extract_digits_after(data, b'R', 7)?;
        let m = extract_digits_after(data, b'M', 1)?;
        let n = extract_digits_after(data, b'N', 1)?;
        let c = extract_digits_after(data, b'C', 1)?;
        let p = extract_digits_after(data, b'P', 1)?;
        let pw = extract_digits_after(data, b'#', 1)?;

        Some(Self {
            tx_hz: parse_ascii_digits(tx)?.checked_mul(100)?,
            rx_hz: parse_ascii_digits(rx)?.checked_mul(100)?,
            mod_tx_rx: match m[0] {
                b'0' => ModMode::Am,
                b'1' => ModMode::Fm,
                _ => return None,
            },
            mod_tx_only: match n[0] {
                b'0' => ModMode::Am,
                b'1' => ModMode::Fm,
                _ => return None,
            },
            on_scan_list: c[0] == b'1',
            preset: PresetId::new(p[0].checked_sub(b'0')?)?,
            power: match pw[0] {
                b'0' => PowerLevel::Lo,
                b'1' => PowerLevel::Med,
                b'2' => PowerLevel::Hi,
                _ => return None,
            },
        })
    }
}

/// `?11` response — general status flags.
/// Wire: `XxJjLldyFf...` plus a trailing `zzz` options byte. Fields (by prefix):
///   `X` — 0=PT, 1=CT
///   `J` — 0=speaker off, 1=on
///   `L` — 0=lamp off, 1=lo, 2=med, 3=hi
///   `d` — installed-options byte: 0=none, 2=EBN-30, 4=EBN-400, 6=both
///         (obsolete values 1/3/5/7 per manual — we expose raw)
///   `F` — 0=temp OK, 1=overtemp
///   trailing 3 digits = options-extra byte (opaque, kept raw)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneralStatus {
    pub text_mode: TextMode,
    pub speaker_on: bool,
    pub lamp: LampLevel,
    pub options_byte: u8,
    pub overtemp: bool,
}

impl GeneralStatus {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let x = extract_digits_after(data, b'X', 1)?;
        let j = extract_digits_after(data, b'J', 1)?;
        let l = extract_digits_after(data, b'L', 1)?;
        let d = extract_digits_after(data, b'd', 1)?;
        let f = extract_digits_after(data, b'F', 1)?;

        Some(Self {
            text_mode: match x[0] {
                b'0' => TextMode::Pt,
                b'1' => TextMode::Ct,
                _ => return None,
            },
            speaker_on: j[0] == b'1',
            lamp: match l[0] {
                b'0' => LampLevel::Off,
                b'1' => LampLevel::Lo,
                b'2' => LampLevel::Med,
                b'3' => LampLevel::Hi,
                _ => return None,
            },
            options_byte: d[0].checked_sub(b'0')?,
            overtemp: f[0] == b'1',
        })
    }

    /// Interpret the options byte per manual ?11: `d2` = EBN-30 installed,
    /// `d4` = EBN-400 installed, `d6` = both. Obsolete values are reported as-is.
    pub fn has_ebn30(&self) -> bool {
        matches!(self.options_byte, 2 | 6)
    }
    pub fn has_ebn400(&self) -> bool {
        matches!(self.options_byte, 4 | 6)
    }
}

// ---------- Helpers ----------

fn strip_prefix<'a>(data: &'a [u8], expect: u8) -> Option<&'a [u8]> {
    let first = *data.first()?;
    if first != expect {
        return None;
    }
    Some(&data[1..])
}

/// Find byte `prefix` in `data` and return the `n` digits immediately after.
/// Returns None if not found, not enough bytes follow, or the bytes aren't
/// ASCII digits.
fn extract_digits_after(data: &[u8], prefix: u8, n: usize) -> Option<&[u8]> {
    let idx = data.iter().position(|&b| b == prefix)?;
    let start = idx + 1;
    let end = start.checked_add(n)?;
    if end > data.len() {
        return None;
    }
    let slice = &data[start..end];
    if !slice.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(slice)
}

fn parse_ascii_digits(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &b in s {
        let d = b.checked_sub(b'0')?;
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(d as u32)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inquiry_encodes_two_digit_zero_padded() {
        assert_eq!(Inquiry::SynthLock.encode(), *b"?01");
        assert_eq!(Inquiry::Rssi.encode(), *b"?03");
        assert_eq!(Inquiry::PresetSnapshot.encode(), *b"?10");
        assert_eq!(Inquiry::AviationMode.encode(), *b"?89");
        assert_eq!(Inquiry::Debug.encode(), *b"?99");
    }

    #[test]
    fn synth_lock_decoder() {
        assert_eq!(SynthLock::from_bytes(b"A0"), Some(SynthLock(false)));
        assert_eq!(SynthLock::from_bytes(b"A1"), Some(SynthLock(true)));
        assert_eq!(SynthLock::from_bytes(b"A2"), None);
        assert_eq!(SynthLock::from_bytes(b""), None);
    }

    #[test]
    fn rssi_decoder() {
        assert_eq!(Rssi::from_bytes(b"N000"), Some(Rssi(0)));
        assert_eq!(Rssi::from_bytes(b"N128"), Some(Rssi(128)));
        assert_eq!(Rssi::from_bytes(b"N255"), Some(Rssi(255)));
        assert_eq!(Rssi::from_bytes(b"N999"), Some(Rssi(255))); // saturates
        assert_eq!(Rssi::from_bytes(b"N12"), None);
        assert_eq!(Rssi::from_bytes(b"X128"), None);
    }

    #[test]
    fn mode_decoder() {
        assert_eq!(Mode::from_bytes(b"U0"), Some(Mode::Receive));
        assert_eq!(Mode::from_bytes(b"U1"), Some(Mode::Transmit));
        assert_eq!(Mode::from_bytes(b"*1"), Some(Mode::Beacon));
        assert_eq!(Mode::from_bytes(b"U2"), None);
    }

    #[test]
    fn squelch_status_decoder() {
        assert_eq!(SquelchStatus::from_bytes(b"[0"), Some(SquelchStatus::Closed));
        assert_eq!(SquelchStatus::from_bytes(b"[1"), Some(SquelchStatus::Broken));
        assert_eq!(SquelchStatus::from_bytes(b"[2"), None);
    }

    #[test]
    fn preset_snapshot_decoder() {
        // Synthetic: preset 3, TX=251.950 MHz, RX=251.950 MHz, FM both,
        // on scan list, high power.
        let wire = b"T2519500R2519500M1N1C1P3#2";
        let snap = PresetSnapshot::from_bytes(wire).unwrap();
        assert_eq!(snap.tx_hz, 251_950_000);
        assert_eq!(snap.rx_hz, 251_950_000);
        assert_eq!(snap.mod_tx_rx, ModMode::Fm);
        assert_eq!(snap.mod_tx_only, ModMode::Fm);
        assert!(snap.on_scan_list);
        assert_eq!(snap.preset.get(), 3);
        assert_eq!(snap.power, PowerLevel::Hi);
    }

    #[test]
    fn preset_snapshot_field_order_insensitive() {
        // Same data, arbitrary order (the parser scans by prefix).
        let wire = b"#0P7C0N0M0R1500000T1500000";
        let snap = PresetSnapshot::from_bytes(wire).unwrap();
        assert_eq!(snap.preset.get(), 7);
        assert_eq!(snap.tx_hz, 150_000_000);
        assert_eq!(snap.power, PowerLevel::Lo);
    }

    #[test]
    fn preset_snapshot_rejects_missing_field() {
        // Missing P field.
        let wire = b"T2519500R2519500M1N1C1#2";
        assert!(PresetSnapshot::from_bytes(wire).is_none());
    }

    #[test]
    fn general_status_decoder() {
        // PT, speaker on, lamp med, no options (d0), temp OK.
        let wire = b"X0J1L2d0F0000";
        let g = GeneralStatus::from_bytes(wire).unwrap();
        assert_eq!(g.text_mode, TextMode::Pt);
        assert!(g.speaker_on);
        assert_eq!(g.lamp, LampLevel::Med);
        assert_eq!(g.options_byte, 0);
        assert!(!g.overtemp);
        assert!(!g.has_ebn30());
        assert!(!g.has_ebn400());
    }

    #[test]
    fn general_status_decodes_ebn30_installed() {
        let wire = b"X0J0L0d2F0001";
        let g = GeneralStatus::from_bytes(wire).unwrap();
        assert!(g.has_ebn30());
        assert!(!g.has_ebn400());
    }

    #[test]
    fn general_status_decodes_overtemp() {
        let wire = b"X1J1L3d6F1255";
        let g = GeneralStatus::from_bytes(wire).unwrap();
        assert_eq!(g.text_mode, TextMode::Ct);
        assert_eq!(g.lamp, LampLevel::Hi);
        assert!(g.overtemp);
        assert!(g.has_ebn30());
        assert!(g.has_ebn400());
    }

    #[test]
    fn general_status_rejects_garbage() {
        assert!(GeneralStatus::from_bytes(b"").is_none());
        assert!(GeneralStatus::from_bytes(b"X9J0L0d0F0000").is_none()); // bad X
        assert!(GeneralStatus::from_bytes(b"X0J0Z0d0F0000").is_none()); // missing L
    }
}
