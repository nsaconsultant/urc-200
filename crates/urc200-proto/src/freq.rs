//! Band-aware frequency type with range + step validation.
//!
//! Per manual §4.6.4 Table 10 (operational frequency-based dependencies):
//!   - Base VHF/UHF: 115.000–173.995 MHz, 225.000–399.995 MHz
//!   - EBN-30 (LVHF): 30.000–90.000 MHz, FM only
//!   - EBN-400:       400.000–420.000 MHz
//!   - ECS-8 aviation: 8.33 kHz tuning in 117.975–136.975 MHz sub-range
//!
//! Wire encoding (per §4.6.4 R/T commands): 6 ASCII digits representing the
//! kHz value, zero-padded. The leading digit is implicitly the band marker —
//! "0" for EBN-30 (30-90 MHz), "4" for EBN-400, 1/2/3 for the base bands.
//! The radio auto-determines a 7th digit based on the configured step grid.
//!
//! NOTE: the precise 7th-digit behavior and the 8.33 kHz ICAO encoding (Table
//! 18) are not exercised yet. They will be verified against a real radio in
//! HIL testing and corrected here if the digit mapping is off.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// Base VHF (115.000–173.995 MHz) or UHF (225.000–399.995 MHz).
    Base,
    /// EBN-30 option: 30.000–90.000 MHz LVHF. FM only.
    Lvhf,
    /// EBN-400 option: 400.000–420.000 MHz.
    Uhf400,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Khz25,
    Khz12_5,
    Khz5,
    /// ECS-8 aviation — uses ICAO 8.33 kHz channel encoding (not a simple Hz step).
    Khz8_33,
}

impl Step {
    /// Nominal step in Hz. For Khz8_33 this is the raw 8,333 Hz width; the wire
    /// encoding follows ICAO rules and is NOT a modulo-aligned multiple of 8333.
    pub fn hz(self) -> u32 {
        match self {
            Step::Khz25 => 25_000,
            Step::Khz12_5 => 12_500,
            Step::Khz5 => 5_000,
            Step::Khz8_33 => 8_333,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freq {
    hz: u32,
    band: Band,
    step: Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreqError {
    OutOfRange { hz: u32, band: Band },
    MisalignedStep { hz: u32, step: Step },
    IncompatibleBandStep { band: Band, step: Step },
}

impl fmt::Display for FreqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FreqError::OutOfRange { hz, band } => {
                write!(f, "{} Hz is outside {:?} band", hz, band)
            }
            FreqError::MisalignedStep { hz, step } => {
                write!(f, "{} Hz is not on the {:?} grid", hz, step)
            }
            FreqError::IncompatibleBandStep { band, step } => {
                write!(f, "{:?} step not supported on {:?} band", step, band)
            }
        }
    }
}

impl std::error::Error for FreqError {}

impl Freq {
    pub fn new(hz: u32, band: Band, step: Step) -> Result<Self, FreqError> {
        if !Self::in_band(hz, band) {
            return Err(FreqError::OutOfRange { hz, band });
        }
        if !Self::band_step_ok(band, step) {
            return Err(FreqError::IncompatibleBandStep { band, step });
        }
        // 8.33 kHz has its own alignment rules; skip modulo check.
        if step != Step::Khz8_33 && hz % step.hz() != 0 {
            return Err(FreqError::MisalignedStep { hz, step });
        }
        Ok(Self { hz, band, step })
    }

    pub fn hz(&self) -> u32 {
        self.hz
    }
    pub fn band(&self) -> Band {
        self.band
    }
    pub fn step(&self) -> Step {
        self.step
    }

    /// 6 ASCII digits = kHz value, zero-padded. See module docs.
    pub(crate) fn encode(&self) -> [u8; 6] {
        let khz = self.hz / 1000;
        let mut out = [b'0'; 6];
        let mut n = khz;
        for slot in out.iter_mut().rev() {
            *slot = b'0' + (n % 10) as u8;
            n /= 10;
        }
        out
    }

    fn in_band(hz: u32, band: Band) -> bool {
        match band {
            Band::Base => {
                (115_000_000..=173_995_000).contains(&hz)
                    || (225_000_000..=399_995_000).contains(&hz)
            }
            Band::Lvhf => (30_000_000..=90_000_000).contains(&hz),
            Band::Uhf400 => (400_000_000..=420_000_000).contains(&hz),
        }
    }

    fn band_step_ok(band: Band, step: Step) -> bool {
        match (band, step) {
            // LVHF: Table 10 shows 25 and 12.5 kHz only; 5 and 8.33 not available.
            (Band::Lvhf, Step::Khz5) | (Band::Lvhf, Step::Khz8_33) => false,
            // 8.33 kHz is only valid inside the base VHF aviation sub-range.
            // We permit it here at the type level and rely on HIL to flag misuse —
            // the radio returns NAK for illegal combinations.
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_vhf_ok() {
        let f = Freq::new(128_500_000, Band::Base, Step::Khz25).unwrap();
        assert_eq!(f.encode(), *b"128500");
    }

    #[test]
    fn base_uhf_ok() {
        let f = Freq::new(251_950_000, Band::Base, Step::Khz25).unwrap();
        assert_eq!(f.encode(), *b"251950");
    }

    #[test]
    fn lvhf_ok_and_band_marker_is_leading_zero() {
        let f = Freq::new(51_950_000, Band::Lvhf, Step::Khz25).unwrap();
        assert_eq!(f.encode(), *b"051950");
    }

    #[test]
    fn uhf400_ok_and_band_marker_is_leading_four() {
        let f = Freq::new(412_500_000, Band::Uhf400, Step::Khz25).unwrap();
        assert_eq!(f.encode(), *b"412500");
    }

    #[test]
    fn out_of_range_rejected() {
        assert!(matches!(
            Freq::new(100_000_000, Band::Base, Step::Khz25),
            Err(FreqError::OutOfRange { .. })
        ));
        assert!(matches!(
            Freq::new(95_000_000, Band::Lvhf, Step::Khz25),
            Err(FreqError::OutOfRange { .. })
        ));
        assert!(matches!(
            Freq::new(425_000_000, Band::Uhf400, Step::Khz25),
            Err(FreqError::OutOfRange { .. })
        ));
    }

    #[test]
    fn misaligned_step_rejected() {
        assert!(matches!(
            Freq::new(251_900_001, Band::Base, Step::Khz25),
            Err(FreqError::MisalignedStep { .. })
        ));
    }

    #[test]
    fn twelve_five_step_accepted() {
        let f = Freq::new(251_937_500, Band::Base, Step::Khz12_5).unwrap();
        assert_eq!(f.hz(), 251_937_500);
        assert_eq!(f.encode(), *b"251937"); // kHz truncates the .5
    }

    #[test]
    fn lvhf_rejects_5khz_and_833() {
        assert!(matches!(
            Freq::new(50_005_000, Band::Lvhf, Step::Khz5),
            Err(FreqError::IncompatibleBandStep { .. })
        ));
        assert!(matches!(
            Freq::new(50_000_000, Band::Lvhf, Step::Khz8_33),
            Err(FreqError::IncompatibleBandStep { .. })
        ));
    }
}
