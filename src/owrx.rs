//! OpenWebRX+'s `settings.json` (in its data directory, `/var/lib/openwebrx` by default):
//! `sdrs` maps a device id to its settings, and each device's `profiles` maps a profile id
//! to `name`, `center_freq` and `samp_rate` (Hz). A profile shows the waterfall from
//! `center_freq - samp_rate/2` to `center_freq + samp_rate/2`, so that whole span has to sit
//! inside one amateur band before the profile may be offered over RF.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::bands::{Band, band_of, fit, mhz, offer_band};

/// Lets anyone listening move a profile's center frequency, and so off the band.
pub const CENTER_CHANGES: &str = "allow_center_freq_changes";

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub sdr: String,
    pub id: String,
    pub name: String,
    pub center: Option<u64>,
    pub rate: Option<u64>,
}

impl Profile {
    pub fn span(&self) -> Option<(u64, u64)> {
        let (c, r) = (self.center?, self.rate?);
        Some((c.checked_sub(r / 2)?, c.checked_add(r - r / 2)?))
    }

    /// The band it may be offered over RF in.
    pub fn band(&self) -> Option<&'static Band> {
        self.span().and_then(|(lo, hi)| offer_band(lo, hi))
    }

    /// Why it can't go over RF, or None if it can.
    pub fn problem(&self) -> Option<String> {
        let Some((lo, hi)) = self.span() else {
            return Some("has no center_freq or samp_rate, so its span is unknown".into());
        };
        if self.band().is_some() {
            return None;
        }
        if let Some(b) = band_of(lo, hi) {
            return Some(format!(
                "{} to {} is in {} at 219-220 MHz, which 97.303(l) keeps to fixed digital message forwarding, so it isn't offered over RF",
                mhz(lo),
                mhz(hi),
                b.name
            ));
        }
        let mut why = format!("{} to {} is not inside one amateur band", mhz(lo), mhz(hi));
        if let Some((c, b)) = fit(self.center?, self.rate?) {
            let _ = write!(why, "; center_freq {c} ({}) would fit {}", mhz(c), b.name);
        }
        Some(why)
    }
}

fn hz(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn sdrs(settings: &Value) -> Result<&Map<String, Value>> {
    match settings.get("sdrs") {
        Some(Value::Object(m)) => Ok(m),
        _ => bail!("no \"sdrs\" object; is this OpenWebRX's settings.json?"),
    }
}

/// Every profile of every device, in file order.
pub fn profiles(settings: &Value) -> Result<Vec<Profile>> {
    let mut out = vec![];
    for (sdr_id, sdr) in sdrs(settings)? {
        let Some(Value::Object(ps)) = sdr.get("profiles") else {
            continue;
        };
        for (id, p) in ps {
            out.push(Profile {
                sdr: sdr_id.clone(),
                id: id.clone(),
                name: p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                center: hz(p.get("center_freq")),
                // a profile without its own rate uses the device's
                rate: hz(p.get("samp_rate")).or_else(|| hz(sdr.get("samp_rate"))),
            });
        }
    }
    Ok(out)
}

pub fn load(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Python's idea of true, which is what OpenWebRX+ applies to its settings.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// OpenWebRX+ looks the setting up per profile, then per device, then globally. Any true
/// value at any of those levels counts, which errs on the side of withholding the link.
pub fn center_changes_allowed(settings: &Value) -> bool {
    let set = |v: &Value| v.get(CENTER_CHANGES).is_some_and(truthy);
    if set(settings) {
        return true;
    }
    let Ok(devs) = sdrs(settings) else {
        return false;
    };
    devs.values().any(|d| {
        set(d)
            || d.get("profiles")
                .and_then(Value::as_object)
                .is_some_and(|ps| ps.values().any(set))
    })
}

/// What `check-owrx` prints: every profile, whether it may go over RF, and why not.
pub fn report(settings: &Value) -> Result<String> {
    let ps = profiles(settings)?;
    let mut s = String::new();
    let mut bad = 0;
    for p in &ps {
        let span = p
            .span()
            .map(|(lo, hi)| format!("{} to {}", mhz(lo), mhz(hi)))
            .unwrap_or_else(|| "span unknown".into());
        match (p.problem(), p.band()) {
            (None, Some(b)) => {
                let _ = writeln!(
                    s,
                    "ok       {}/{} {:?}: {span}, {}",
                    p.sdr, p.id, p.name, b.name
                );
            }
            (why, _) => {
                bad += 1;
                let _ = writeln!(
                    s,
                    "outside  {}/{} {:?}: {}",
                    p.sdr,
                    p.id,
                    p.name,
                    why.unwrap_or_default()
                );
            }
        }
    }
    if center_changes_allowed(settings) {
        let _ = writeln!(
            s,
            "{CENTER_CHANGES} is on: listeners can move any profile off the band"
        );
    }
    let _ = writeln!(
        s,
        "{} of {} profiles may be offered over RF",
        ps.len() - bad,
        ps.len()
    );
    Ok(s)
}

/// A copy for the mesh-facing instance: profiles outside the amateur bands removed (or,
/// with `recenter`, moved inside their band when it is wide enough), devices left with no
/// profiles removed, and center frequency changes turned off. Returns what was changed.
pub fn filtered(settings: &Value, recenter: bool) -> Result<(Value, Vec<String>)> {
    let mut out = settings.clone();
    let mut changes = vec![];
    let ps = profiles(settings)?;
    let Some(Value::Object(devs)) = out.get_mut("sdrs") else {
        unreachable!("profiles() checked sdrs is an object");
    };
    for p in ps.iter().filter(|p| p.problem().is_some()) {
        let Some(Value::Object(profs)) = devs.get_mut(&p.sdr).and_then(|d| d.get_mut("profiles"))
        else {
            continue;
        };
        let moved = recenter
            .then(|| fit(p.center?, p.rate?))
            .flatten()
            .filter(|_| p.span().is_some());
        match (moved, profs.get_mut(&p.id)) {
            (Some((c, b)), Some(Value::Object(prof))) => {
                prof.insert("center_freq".into(), c.into());
                // the receiver opens on start_freq, which has to be inside the new span
                let half = p.rate.unwrap_or(0) / 2;
                if hz(prof.get("start_freq")).is_some_and(|f| f < c - half || f > c + half) {
                    prof.insert("start_freq".into(), c.into());
                }
                changes.push(format!(
                    "moved {}/{} {:?} to center {} inside {}",
                    p.sdr,
                    p.id,
                    p.name,
                    mhz(c),
                    b.name
                ));
            }
            _ => {
                profs.remove(&p.id);
                changes.push(format!(
                    "removed {}/{} {:?}: {}",
                    p.sdr,
                    p.id,
                    p.name,
                    p.problem().unwrap_or_default()
                ));
            }
        }
    }
    devs.retain(|id, d| {
        let empty = d
            .get("profiles")
            .and_then(Value::as_object)
            .is_none_or(|m| m.is_empty());
        if empty {
            changes.push(format!("removed device {id}: no amateur profiles left"));
        }
        !empty
    });
    for d in devs.values_mut() {
        turn_off_center_changes(d);
        if let Some(Value::Object(profs)) = d.get_mut("profiles") {
            profs.values_mut().for_each(turn_off_center_changes);
        }
    }
    if let Value::Object(top) = &mut out {
        if top.get(CENTER_CHANGES) != Some(&Value::Bool(false)) {
            changes.push(format!("set {CENTER_CHANGES} to false"));
        }
        top.insert(CENTER_CHANGES.into(), false.into());
    }
    Ok((out, changes))
}

fn turn_off_center_changes(v: &mut Value) {
    if let Some(o) = v.as_object_mut()
        && o.contains_key(CENTER_CHANGES)
    {
        o.insert(CENTER_CHANGES.into(), false.into());
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    /// Shaped like OpenWebRX+'s own defaults, plus a NOAA profile.
    pub fn sample() -> Value {
        json!({
            "version": 8,
            "receiver_name": "TST test receiver",
            "allow_center_freq_changes": true,
            "sdrs": {
                "rtlsdr": {
                    "name": "RTL-SDR",
                    "type": "rtl_sdr",
                    "rf_gain": 29,
                    "profiles": {
                        "2m": {"name": "2m", "center_freq": 145000000, "rf_gain": 29,
                               "samp_rate": 2048000, "start_freq": 145725000, "start_mod": "nfm"},
                        "70cm": {"name": "70cm", "center_freq": 438800000, "rf_gain": 29,
                                 "samp_rate": 2400000, "start_freq": 439275000, "start_mod": "nfm"},
                        "wx": {"name": "NOAA", "center_freq": 162475000,
                               "samp_rate": 1024000, "start_freq": 162550000, "start_mod": "nfm"}
                    }
                },
                "airband": {
                    "name": "Airband",
                    "type": "rtl_sdr",
                    "samp_rate": 2400000,
                    "profiles": {
                        "air": {"name": "Airband", "center_freq": "119000000"}
                    }
                }
            }
        })
    }

    #[test]
    fn reads_profiles_and_their_spans() {
        let ps = profiles(&sample()).unwrap();
        assert_eq!(ps.len(), 4);
        let two = ps.iter().find(|p| p.id == "2m").unwrap();
        assert_eq!(two.span(), Some((143_976_000, 146_024_000)));
        let air = ps.iter().find(|p| p.id == "air").unwrap();
        assert_eq!(air.rate, Some(2_400_000), "falls back to the device's rate");
        assert_eq!(air.center, Some(119_000_000), "numbers given as strings");
        assert!(profiles(&json!({"version": 8})).is_err());
    }

    #[test]
    fn report_names_the_profiles_that_cannot_go_over_rf() {
        let r = report(&sample()).unwrap();
        assert!(r.contains("ok       rtlsdr/70cm"), "{r}");
        assert!(
            r.contains("outside  rtlsdr/2m"),
            "spills below 144 MHz: {r}"
        );
        assert!(
            r.contains("center_freq 145024000 (145.024 MHz) would fit 2 m"),
            "{r}"
        );
        assert!(r.contains("outside  rtlsdr/wx"), "{r}");
        assert!(r.contains("outside  airband/air"), "{r}");
        assert!(r.contains("allow_center_freq_changes is on"), "{r}");
        assert!(r.contains("1 of 4 profiles"), "{r}");
    }

    #[test]
    fn filtered_copy_keeps_only_amateur_profiles() {
        let (out, changes) = filtered(&sample(), false).unwrap();
        let ps = profiles(&out).unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].id, "70cm");
        assert!(out["sdrs"].get("airband").is_none(), "device left empty");
        assert_eq!(out[CENTER_CHANGES], json!(false));
        assert_eq!(
            out["receiver_name"],
            json!("TST test receiver"),
            "rest kept"
        );
        assert!(report(&out).unwrap().contains("1 of 1 profiles"));
        assert_eq!(changes.len(), 5, "{changes:?}");
    }

    #[test]
    fn center_changes_count_at_any_level_and_any_truthy_value() {
        let (mut out, _) = filtered(&sample(), false).unwrap();
        assert!(!center_changes_allowed(&out));
        out["sdrs"]["rtlsdr"]["profiles"]["70cm"][CENTER_CHANGES] = json!(1);
        assert!(center_changes_allowed(&out), "per profile, as a number");
        let (cleared, _) = filtered(&out, false).unwrap();
        assert!(!center_changes_allowed(&cleared));
        out["sdrs"]["rtlsdr"]["profiles"]["70cm"][CENTER_CHANGES] = json!(0);
        out["sdrs"]["rtlsdr"][CENTER_CHANGES] = json!("yes");
        assert!(center_changes_allowed(&out), "per device, as a string");
        out["sdrs"]["rtlsdr"][CENTER_CHANGES] = json!("");
        assert!(!center_changes_allowed(&out));
    }

    #[test]
    fn huge_or_fixed_only_spans_are_not_offered() {
        let p = Profile {
            sdr: "a".into(),
            id: "b".into(),
            name: "x".into(),
            center: Some(u64::MAX - 10),
            rate: Some(2_400_000),
        };
        assert_eq!(p.span(), None);
        assert!(p.problem().is_some());
        let fixed = Profile {
            center: Some(219_500_000),
            rate: Some(250_000),
            ..p
        };
        assert!(fixed.problem().unwrap().contains("97.303(l)"));
    }

    #[test]
    fn recentering_moves_profiles_that_fit_their_band() {
        let (out, changes) = filtered(&sample(), true).unwrap();
        let ps = profiles(&out).unwrap();
        let two = ps.iter().find(|p| p.id == "2m").unwrap();
        assert_eq!(two.center, Some(145_024_000));
        assert!(two.problem().is_none());
        assert_eq!(
            out["sdrs"]["rtlsdr"]["profiles"]["2m"]["start_freq"],
            json!(145725000)
        );
        assert!(ps.iter().all(|p| p.problem().is_none()));
        assert!(ps.iter().all(|p| p.id != "wx"), "NOAA is not in any band");
        assert!(changes.iter().any(|c| c.starts_with("moved rtlsdr/2m")));
    }
}
