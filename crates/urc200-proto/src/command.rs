//! Table 11 remote transceiver operation commands (§4.6.4).
//!
//! Every variant in [`OpCommand`] encodes to a byte sequence the radio accepts
//! over RS-232. No terminator is emitted — the URC-200 parses greedily by
//! command prefix and fixed-width argument.

use crate::freq::Freq;

/// Preset channel index 0-9 (P0-P9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetId(u8);

impl PresetId {
    pub fn new(n: u8) -> Option<Self> {
        if n <= 9 {
            Some(Self(n))
        } else {
            None
        }
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LampLevel {
    Off,
    Lo,
    Med,
    Hi,
}

impl LampLevel {
    fn digit(self) -> u8 {
        match self {
            LampLevel::Off => b'0',
            LampLevel::Lo => b'1',
            LampLevel::Med => b'2',
            LampLevel::Hi => b'3',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModMode {
    Am,
    Fm,
}

impl ModMode {
    fn digit(self) -> u8 {
        match self {
            ModMode::Am => b'0',
            ModMode::Fm => b'1',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMode {
    /// Plain Text (voice).
    Pt,
    /// Cipher Text (data).
    Ct,
}

impl TextMode {
    fn digit(self) -> u8 {
        match self {
            TextMode::Pt => b'0',
            TextMode::Ct => b'1',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerLevel {
    Lo,
    Med,
    Hi,
}

impl PowerLevel {
    fn digit(self) -> u8 {
        match self {
            PowerLevel::Lo => b'0',
            PowerLevel::Med => b'1',
            PowerLevel::Hi => b'2',
        }
    }
}

/// 150 Hz tone-squelch modes (EBN-30 / LVHF option only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneSquelch {
    Off,
    RxOnly,
    TxOnly,
    Both,
}

impl ToneSquelch {
    fn digit(self) -> u8 {
        match self {
            ToneSquelch::Off => b'0',
            ToneSquelch::RxOnly => b'1',
            ToneSquelch::TxOnly => b'2',
            ToneSquelch::Both => b'3',
        }
    }
}

/// Every Table 11 remote-operation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCommand {
    /// `Z` — resync after NAK/timeout.
    Zap,
    /// `$xxx` — remote squelch 0-255 (overrides front-panel knob).
    Squelch(u8),
    /// `I` — initialize all channels to defaults.
    ChannelInit,
    /// `L0..L3` — backlight Off / Lo / Med / Hi.
    Lamp(LampLevel),
    /// `J0`/`J1` — internal speaker Off/On.
    Speaker(bool),
    /// `K` — self-calibration (requires 50Ω load on antenna port).
    Calibrate,
    /// `P0..P9` — select preset channel.
    Preset(PresetId),
    /// `Rxxxxxx` — set receive frequency on current preset.
    SetRx(Freq),
    /// `Txxxxxx` — set transmit frequency on current preset.
    SetTx(Freq),
    /// `M0`/`M1` — modulation mode for both TX and RX (AM/FM).
    ModTxRx(ModMode),
    /// `N0`/`N1` — modulation mode for TX only.
    ModTxOnly(ModMode),
    /// `X0`/`X1` — Plain Text / Cipher Text.
    Text(TextMode),
    /// `Q` — store current settings to EEPROM for the current preset.
    StorePreset,
    /// `C0`/`C1` — current preset is off/on the scan list.
    ScanListMember(bool),
    /// `#0..#2` — transmitter power (Lo / Med / Hi).
    Power(PowerLevel),
    /// `S0`/`S1` — scan mode off/on. NAK'd if fewer than 2 channels on scan list.
    Scan(bool),
    /// `*0`/`*1` — beacon mode off/on.
    Beacon(bool),
    /// `+` — re-enable the physical keypad while in remote mode.
    ReleaseKeypad,
    /// `B` — key the transmitter.
    Transmit,
    /// `E` — return to receive.
    Receive,
    /// `>0..>3` — 150 Hz tone squelch (LVHF option only).
    ToneSquelch(ToneSquelch),
}

impl OpCommand {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match *self {
            Self::Zap => out.push(b'Z'),
            Self::Squelch(n) => {
                out.push(b'$');
                push_3digit(out, n as u32);
            }
            Self::ChannelInit => out.push(b'I'),
            Self::Lamp(l) => {
                out.push(b'L');
                out.push(l.digit());
            }
            Self::Speaker(on) => {
                out.push(b'J');
                out.push(if on { b'1' } else { b'0' });
            }
            Self::Calibrate => out.push(b'K'),
            Self::Preset(p) => {
                out.push(b'P');
                out.push(b'0' + p.get());
            }
            Self::SetRx(f) => {
                out.push(b'R');
                out.extend_from_slice(&f.encode());
            }
            Self::SetTx(f) => {
                out.push(b'T');
                out.extend_from_slice(&f.encode());
            }
            Self::ModTxRx(m) => {
                out.push(b'M');
                out.push(m.digit());
            }
            Self::ModTxOnly(m) => {
                out.push(b'N');
                out.push(m.digit());
            }
            Self::Text(t) => {
                out.push(b'X');
                out.push(t.digit());
            }
            Self::StorePreset => out.push(b'Q'),
            Self::ScanListMember(on) => {
                out.push(b'C');
                out.push(if on { b'1' } else { b'0' });
            }
            Self::Power(p) => {
                out.push(b'#');
                out.push(p.digit());
            }
            Self::Scan(on) => {
                out.push(b'S');
                out.push(if on { b'1' } else { b'0' });
            }
            Self::Beacon(on) => {
                out.push(b'*');
                out.push(if on { b'1' } else { b'0' });
            }
            Self::ReleaseKeypad => out.push(b'+'),
            Self::Transmit => out.push(b'B'),
            Self::Receive => out.push(b'E'),
            Self::ToneSquelch(t) => {
                out.push(b'>');
                out.push(t.digit());
            }
        }
    }
}

fn push_3digit(out: &mut Vec<u8>, n: u32) {
    out.push(b'0' + ((n / 100) % 10) as u8);
    out.push(b'0' + ((n / 10) % 10) as u8);
    out.push(b'0' + (n % 10) as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freq::{Band, Step};

    #[test]
    fn single_character_commands() {
        assert_eq!(OpCommand::Zap.encode(), b"Z");
        assert_eq!(OpCommand::ChannelInit.encode(), b"I");
        assert_eq!(OpCommand::Calibrate.encode(), b"K");
        assert_eq!(OpCommand::StorePreset.encode(), b"Q");
        assert_eq!(OpCommand::ReleaseKeypad.encode(), b"+");
        assert_eq!(OpCommand::Transmit.encode(), b"B");
        assert_eq!(OpCommand::Receive.encode(), b"E");
    }

    #[test]
    fn lamp_levels() {
        assert_eq!(OpCommand::Lamp(LampLevel::Off).encode(), b"L0");
        assert_eq!(OpCommand::Lamp(LampLevel::Lo).encode(), b"L1");
        assert_eq!(OpCommand::Lamp(LampLevel::Med).encode(), b"L2");
        assert_eq!(OpCommand::Lamp(LampLevel::Hi).encode(), b"L3");
    }

    #[test]
    fn speaker_toggle() {
        assert_eq!(OpCommand::Speaker(false).encode(), b"J0");
        assert_eq!(OpCommand::Speaker(true).encode(), b"J1");
    }

    #[test]
    fn squelch_is_three_digits() {
        assert_eq!(OpCommand::Squelch(0).encode(), b"$000");
        assert_eq!(OpCommand::Squelch(7).encode(), b"$007");
        assert_eq!(OpCommand::Squelch(42).encode(), b"$042");
        assert_eq!(OpCommand::Squelch(255).encode(), b"$255");
    }

    #[test]
    fn preset_bounds() {
        for n in 0..=9 {
            let p = PresetId::new(n).unwrap();
            assert_eq!(OpCommand::Preset(p).encode(), format!("P{n}").as_bytes());
        }
        assert!(PresetId::new(10).is_none());
    }

    #[test]
    fn modulation_both_directions() {
        assert_eq!(OpCommand::ModTxRx(ModMode::Am).encode(), b"M0");
        assert_eq!(OpCommand::ModTxRx(ModMode::Fm).encode(), b"M1");
        assert_eq!(OpCommand::ModTxOnly(ModMode::Am).encode(), b"N0");
        assert_eq!(OpCommand::ModTxOnly(ModMode::Fm).encode(), b"N1");
    }

    #[test]
    fn text_mode() {
        assert_eq!(OpCommand::Text(TextMode::Pt).encode(), b"X0");
        assert_eq!(OpCommand::Text(TextMode::Ct).encode(), b"X1");
    }

    #[test]
    fn power_levels() {
        assert_eq!(OpCommand::Power(PowerLevel::Lo).encode(), b"#0");
        assert_eq!(OpCommand::Power(PowerLevel::Med).encode(), b"#1");
        assert_eq!(OpCommand::Power(PowerLevel::Hi).encode(), b"#2");
    }

    #[test]
    fn scan_and_beacon() {
        assert_eq!(OpCommand::Scan(false).encode(), b"S0");
        assert_eq!(OpCommand::Scan(true).encode(), b"S1");
        assert_eq!(OpCommand::Beacon(false).encode(), b"*0");
        assert_eq!(OpCommand::Beacon(true).encode(), b"*1");
    }

    #[test]
    fn scan_list_membership() {
        assert_eq!(OpCommand::ScanListMember(false).encode(), b"C0");
        assert_eq!(OpCommand::ScanListMember(true).encode(), b"C1");
    }

    #[test]
    fn tone_squelch() {
        assert_eq!(OpCommand::ToneSquelch(ToneSquelch::Off).encode(), b">0");
        assert_eq!(OpCommand::ToneSquelch(ToneSquelch::RxOnly).encode(), b">1");
        assert_eq!(OpCommand::ToneSquelch(ToneSquelch::TxOnly).encode(), b">2");
        assert_eq!(OpCommand::ToneSquelch(ToneSquelch::Both).encode(), b">3");
    }

    #[test]
    fn frequency_commands() {
        let f = Freq::new(251_950_000, Band::Base, Step::Khz25).unwrap();
        assert_eq!(OpCommand::SetRx(f).encode(), b"R251950");
        assert_eq!(OpCommand::SetTx(f).encode(), b"T251950");

        let lvhf = Freq::new(51_950_000, Band::Lvhf, Step::Khz25).unwrap();
        assert_eq!(OpCommand::SetRx(lvhf).encode(), b"R051950");

        let uhf400 = Freq::new(412_500_000, Band::Uhf400, Step::Khz25).unwrap();
        assert_eq!(OpCommand::SetTx(uhf400).encode(), b"T412500");
    }

    #[test]
    fn encode_into_accumulates() {
        let mut buf = Vec::new();
        OpCommand::Preset(PresetId::new(3).unwrap()).encode_into(&mut buf);
        OpCommand::Transmit.encode_into(&mut buf);
        assert_eq!(buf, b"P3B");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::freq::{Band, Step};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn squelch_always_4_bytes(n in 0u8..=255) {
            let e = OpCommand::Squelch(n).encode();
            prop_assert_eq!(e.len(), 4);
            prop_assert_eq!(e[0], b'$');
            prop_assert!(e[1..].iter().all(|b| b.is_ascii_digit()));
            let parsed: u32 = std::str::from_utf8(&e[1..]).unwrap().parse().unwrap();
            prop_assert_eq!(parsed, n as u32);
        }

        #[test]
        fn preset_round_trip(n in 0u8..=9) {
            let p = PresetId::new(n).unwrap();
            let e = OpCommand::Preset(p).encode();
            prop_assert_eq!(e.len(), 2);
            prop_assert_eq!(e[0], b'P');
            prop_assert_eq!(e[1], b'0' + n);
        }

        #[test]
        fn base_freq_encodes_to_seven_ascii_digits(khz in 115_000u32..=173_995u32) {
            let hz = (khz / 25) * 25 * 1000;
            if !(115_000_000..=173_995_000).contains(&hz) { return Ok(()); }
            let f = Freq::new(hz, Band::Base, Step::Khz25).unwrap();
            let e = OpCommand::SetRx(f).encode();
            prop_assert_eq!(e.len(), 7);
            prop_assert_eq!(e[0], b'R');
            prop_assert!(e[1..].iter().all(|b| b.is_ascii_digit()));
            let parsed: u32 = std::str::from_utf8(&e[1..]).unwrap().parse().unwrap();
            prop_assert_eq!(parsed, hz / 1000);
        }

        #[test]
        fn lvhf_always_leading_zero(khz in 30_000u32..=90_000u32) {
            let hz = (khz / 25) * 25 * 1000;
            if !(30_000_000..=90_000_000).contains(&hz) { return Ok(()); }
            let f = Freq::new(hz, Band::Lvhf, Step::Khz25).unwrap();
            let e = OpCommand::SetRx(f).encode();
            prop_assert_eq!(e[0], b'R');
            prop_assert_eq!(e[1], b'0', "LVHF must encode with leading '0' band marker");
        }

        #[test]
        fn uhf400_always_leading_four(khz in 400_000u32..=420_000u32) {
            let hz = (khz / 25) * 25 * 1000;
            if !(400_000_000..=420_000_000).contains(&hz) { return Ok(()); }
            let f = Freq::new(hz, Band::Uhf400, Step::Khz25).unwrap();
            let e = OpCommand::SetRx(f).encode();
            prop_assert_eq!(e[0], b'R');
            prop_assert_eq!(e[1], b'4', "UHF-400 must encode with leading '4' band marker");
        }
    }
}
