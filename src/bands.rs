//! US amateur allocations, for keeping non-amateur spectrum off RF links (97.113(c)).
//!
//! Source: 47 CFR 97.301 (a) to (f), ITU Region 2, every licence class combined, with the
//! 60 m channels from 97.303(h)(3), as published on 2026-11-19. 219-220 MHz may be measured,
//! but 97.303(l)(1) limits it to fixed point-to-point digital message forwarding, so no
//! web receiver offers it over RF.

use anyhow::{Result, bail};

pub struct Band {
    pub name: &'static str,
    pub lo: u64,
    pub hi: u64,
    /// whether a web receiver may offer it over RF
    pub offer: bool,
}

const fn khz(name: &'static str, lo: u64, hi: u64) -> Band {
    Band {
        name,
        lo: lo * 1000,
        hi: hi * 1000,
        offer: true,
    }
}

/// 60 m channels are 2.8 kHz wide around their center frequencies.
const fn ch60(center_hz: u64) -> Band {
    Band {
        name: "60 m channel",
        lo: center_hz - 1400,
        hi: center_hz + 1400,
        offer: true,
    }
}

pub const BANDS: &[Band] = &[
    Band {
        name: "2200 m",
        lo: 135_700,
        hi: 137_800,
        offer: true,
    },
    khz("630 m", 472, 479),
    khz("160 m", 1800, 2000),
    khz("80 m", 3500, 4000),
    ch60(5_332_000),
    ch60(5_348_000),
    Band {
        name: "60 m",
        lo: 5_351_500,
        hi: 5_366_500,
        offer: true,
    },
    ch60(5_373_000),
    ch60(5_405_000),
    khz("40 m", 7000, 7300),
    khz("30 m", 10_100, 10_150),
    khz("20 m", 14_000, 14_350),
    khz("17 m", 18_068, 18_168),
    khz("15 m", 21_000, 21_450),
    khz("12 m", 24_890, 24_990),
    khz("10 m", 28_000, 29_700),
    khz("6 m", 50_000, 54_000),
    khz("2 m", 144_000, 148_000),
    Band {
        name: "1.25 m",
        lo: 219_000_000,
        hi: 220_000_000,
        offer: false,
    },
    khz("1.25 m", 222_000, 225_000),
    khz("70 cm", 420_000, 450_000),
    khz("33 cm", 902_000, 928_000),
    khz("23 cm", 1_240_000, 1_300_000),
    khz("13 cm", 2_300_000, 2_310_000),
    khz("13 cm", 2_390_000, 2_450_000),
    khz("5 cm", 5_650_000, 5_925_000),
    khz("3 cm", 10_000_000, 10_500_000),
];

/// The amateur band that holds all of `lo..=hi`, if one does.
pub fn band_of(lo: u64, hi: u64) -> Option<&'static Band> {
    if lo > hi {
        return None;
    }
    BANDS.iter().find(|b| b.lo <= lo && hi <= b.hi)
}

/// Like `band_of`, but only bands a web receiver may offer over RF.
pub fn offer_band(lo: u64, hi: u64) -> Option<&'static Band> {
    band_of(lo, hi).filter(|b| b.offer)
}

/// Where a span `width` wide could sit inside the band that holds `center`, if that band
/// is wide enough: the center frequency to move to.
pub fn fit(center: u64, width: u64) -> Option<(u64, &'static Band)> {
    let half = width / 2;
    BANDS
        .iter()
        .find(|b| b.offer && b.lo <= center && center <= b.hi && b.hi - b.lo >= width)
        .map(|b| (center.clamp(b.lo + half, b.hi - (width - half)), b))
}

/// "146.520 MHz", trimmed to the precision that matters.
pub fn mhz(hz: u64) -> String {
    let s = format!("{:.6}", hz as f64 / 1e6);
    let s = s.trim_end_matches('0');
    let s = s.strip_suffix('.').unwrap_or(s);
    format!("{s} MHz")
}

/// "146.52M", "146.52 MHz", "16k", "2.4m", "146520000" or "146520000Hz", in Hz.
pub fn parse_hz(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_lowercase().replace(' ', "");
    let t = t.strip_suffix("hz").unwrap_or(&t);
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1e3),
        Some('m') => (&t[..t.len() - 1], 1e6),
        Some('g') => (&t[..t.len() - 1], 1e9),
        _ => (t, 1.0),
    };
    let v: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("{s:?} is not a frequency; try 146.52M or 16k"))?;
    let hz = (v * mult).round();
    if !hz.is_finite() || hz < 1.0 || hz > 1e12 {
        bail!("{s:?} is out of range for a frequency");
    }
    Ok(hz as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_inside_one_band_pass_and_others_do_not() {
        assert_eq!(band_of(144_000_000, 148_000_000).unwrap().name, "2 m");
        assert_eq!(band_of(146_500_000, 146_540_000).unwrap().name, "2 m");
        // the OpenWebRX default 2 m profile spills 24 kHz below the band
        assert!(band_of(143_976_000, 146_024_000).is_none());
        // NOAA weather radio, aircraft and FM broadcast are not amateur
        assert!(band_of(162_400_000, 162_550_000).is_none());
        assert!(band_of(118_000_000, 120_000_000).is_none());
        assert!(band_of(98_000_000, 98_200_000).is_none());
        // a span across the gap in 1.25 m is not inside one band
        assert!(band_of(219_500_000, 222_500_000).is_none());
        assert_eq!(band_of(223_000_000, 224_000_000).unwrap().name, "1.25 m");
        assert_eq!(band_of(5_330_700, 5_333_300).unwrap().name, "60 m channel");
        assert!(
            band_of(146_000_000, 145_000_000).is_none(),
            "backwards span"
        );
        assert_eq!(band_of(219_100_000, 219_200_000).unwrap().name, "1.25 m");
        assert!(offer_band(219_100_000, 219_200_000).is_none(), "97.303(l)");
        assert!(offer_band(223_000_000, 224_000_000).is_some());
        assert!(band_of(5_340_000, 5_350_000).is_none());
        for (name, lo, hi) in [
            ("160 m", 1.9e6, 1.95e6),
            ("40 m", 7.074e6, 7.077e6),
            ("10 m", 28.0e6, 29.7e6),
            ("6 m", 50.1e6, 50.4e6),
            ("70 cm", 438.0e6, 440.4e6),
            ("33 cm", 915.0e6, 917.0e6),
            ("23 cm", 1.29e9, 1.296e9),
        ] {
            assert_eq!(band_of(lo as u64, hi as u64).unwrap().name, name);
        }
    }

    #[test]
    fn fit_moves_a_span_just_inside_the_band() {
        let (c, b) = fit(145_000_000, 2_048_000).unwrap();
        assert_eq!(b.name, "2 m");
        assert_eq!(c, 145_024_000);
        assert!(band_of(c - 1_024_000, c + 1_024_000).is_some());
        assert!(fit(162_475_000, 2_400_000).is_none(), "not in a band");
        assert!(
            fit(14_100_000, 2_400_000).is_none(),
            "20 m is narrower than that"
        );
    }

    #[test]
    fn frequencies_parse_with_suffixes() {
        assert_eq!(parse_hz("146.52M").unwrap(), 146_520_000);
        assert_eq!(parse_hz("146.52 MHz").unwrap(), 146_520_000);
        assert_eq!(parse_hz("16k").unwrap(), 16_000);
        assert_eq!(parse_hz("1.2G").unwrap(), 1_200_000_000);
        assert_eq!(parse_hz("146520000").unwrap(), 146_520_000);
        assert!(parse_hz("two meters").is_err());
        assert!(parse_hz("-5k").is_err());
        assert_eq!(mhz(146_520_000), "146.52 MHz");
        assert_eq!(mhz(145_000_000), "145 MHz");
    }
}
