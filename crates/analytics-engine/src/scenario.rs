//! The common scenario format, applied (contract v28; W12.8;
//! sdk-contract/the-common-scenario-format-contract; rulings 566 and 581 to
//! 584): a scenario set's axes, the points their product generates -- each
//! keyed by its axis positions -- and each point's shocks applied to a spot,
//! a volatility, a rate and the valuation time.
//!
//! The inputs document carries the set as the component resolved it, inline
//! (`Job.scenario_set`, its row key `inline`) or saved (`Job.scenario_ref`,
//! the lake's row key and version), its tenors already in days, so the
//! answer's hash covers exactly what was applied. A saved single
//! `Scenario` is a set of one axis with one step.

use serde_json::Value;

use crate::case::{decimal, CaseError, Result};

pub const SPOT: &str = "SHOCK_OBJECT_SPOT";
pub const VOL_SURFACE: &str = "SHOCK_OBJECT_VOL_SURFACE";
pub const RATE: &str = "SHOCK_OBJECT_RATE";
pub const TIME: &str = "SHOCK_OBJECT_TIME";

pub const RELATIVE: &str = "SHOCK_FORM_RELATIVE";
pub const ABSOLUTE: &str = "SHOCK_FORM_ABSOLUTE";
pub const PARALLEL: &str = "SHOCK_FORM_PARALLEL";
pub const BUCKETED: &str = "SHOCK_FORM_BUCKETED";
pub const TO_SETTLEMENT: &str = "SHOCK_FORM_TO_SETTLEMENT";

pub const PERCENT: &str = "SHOCK_UNIT_PERCENT";
pub const PRICE: &str = "SHOCK_UNIT_PRICE";
pub const VOL_POINTS: &str = "SHOCK_UNIT_VOL_POINTS";
pub const BASIS_POINTS: &str = "SHOCK_UNIT_BASIS_POINTS";
pub const DAYS: &str = "SHOCK_UNIT_DAYS";
pub const MINUTES: &str = "SHOCK_UNIT_MINUTES";

/// Minutes in the year time is counted over (G16).
pub const MINUTES_A_YEAR: f64 = 525_600.0;

/// One shock, as the document carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct Shock {
    pub object: String,
    pub subject_id: String,
    pub every: bool,
    pub form: String,
    pub unit: String,
    pub value: f64,
    /// A bucketed shock's (days, value), in order of days.
    pub buckets: Vec<(f64, f64)>,
}

/// A scenario set: its key, as the answer names its points, and its axes,
/// each a list of steps, each step its shocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Set {
    pub row_key: String,
    pub version: u64,
    pub axes: Vec<Vec<Vec<Shock>>>,
}

/// Why a shock as stated cannot stand: its form or unit is not one its
/// object takes (ruling 581: stated, never inferred), or its target is
/// missing or doubled (583). The words name the field.
pub fn refused(shock: &Shock) -> Option<String> {
    let forms: &[&str] = match shock.object.as_str() {
        SPOT => &[RELATIVE, ABSOLUTE],
        VOL_SURFACE | RATE => &[PARALLEL, BUCKETED],
        TIME => &[ABSOLUTE, TO_SETTLEMENT],
        _ => return Some("object: a shock names its object".into()),
    };
    if !forms.contains(&shock.form.as_str()) {
        return Some(format!(
            "form: {} takes {}, not {:?}",
            shock.object,
            forms.join(" or "),
            shock.form
        ));
    }
    let units: &[&str] = match (shock.object.as_str(), shock.form.as_str()) {
        (SPOT, RELATIVE) => &[PERCENT],
        (SPOT, ABSOLUTE) => &[PRICE],
        (VOL_SURFACE, _) => &[VOL_POINTS],
        (RATE, _) => &[BASIS_POINTS],
        (TIME, ABSOLUTE) => &[DAYS, MINUTES],
        _ => &[],
    };
    if !units.is_empty() && !units.contains(&shock.unit.as_str()) {
        return Some(format!(
            "unit: {} {} is in {}, not {:?}",
            shock.object,
            shock.form,
            units.join(" or "),
            shock.unit
        ));
    }
    if shock.object != TIME {
        if shock.every && !shock.subject_id.is_empty() {
            return Some("target: every, or a subject_id, never both".into());
        }
        if !shock.every && shock.subject_id.is_empty() {
            return Some("target: names a subject_id, or every".into());
        }
    }
    if shock.form == BUCKETED && shock.buckets.is_empty() {
        return Some("buckets: a bucketed shock carries its buckets".into());
    }
    None
}

fn text(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn items<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn number(value: Option<&Value>, what: &str) -> Result<f64> {
    match value {
        None | Some(Value::Null) => Ok(0.0),
        some => Ok(decimal(some, what)?.to_f64()),
    }
}

impl Set {
    /// The set an inputs document carries as `scenario`, or none.
    pub fn read(inputs: &Value) -> Result<Option<Set>> {
        let held = match inputs.get("scenario") {
            None | Some(Value::Null) => return Ok(None),
            Some(held) => held,
        };
        let mut axes = Vec::new();
        for axis in items(held, "axes") {
            let mut steps = Vec::new();
            for step in items(axis, "steps") {
                let mut shocks = Vec::new();
                for shock in items(step, "shocks") {
                    let mut buckets = Vec::new();
                    for bucket in items(shock, "buckets") {
                        buckets.push((
                            number(bucket.get("days"), "a bucket's days")?,
                            number(bucket.get("value"), "a bucket's value")?,
                        ));
                    }
                    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
                    let read = Shock {
                        object: text(shock, "object"),
                        subject_id: text(shock, "subject_id"),
                        every: shock.get("every").and_then(Value::as_bool).unwrap_or(false),
                        form: text(shock, "form"),
                        unit: text(shock, "unit"),
                        value: number(shock.get("value"), "a shock's value")?,
                        buckets,
                    };
                    if let Some(why) = refused(&read) {
                        return Err(CaseError(format!("scenario: {why}")));
                    }
                    shocks.push(read);
                }
                steps.push(shocks);
            }
            if steps.is_empty() {
                return Err(CaseError("scenario: an axis has one step or more".into()));
            }
            axes.push(steps);
        }
        if axes.is_empty() {
            return Err(CaseError("scenario: a set has one axis or more".into()));
        }
        Ok(Some(Set {
            row_key: text(held, "row_key"),
            version: text(held, "version").parse().unwrap_or(0),
            axes,
        }))
    }

    /// Every point the axes generate (ruling 582), in order -- the first axis
    /// slowest -- each its axis positions and its shocks, applied together.
    pub fn points(&self) -> Vec<(Vec<u32>, Vec<&Shock>)> {
        let mut out: Vec<(Vec<u32>, Vec<&Shock>)> = vec![(Vec::new(), Vec::new())];
        for axis in &self.axes {
            let mut next = Vec::with_capacity(out.len() * axis.len());
            for (positions, shocks) in &out {
                for (i, step) in axis.iter().enumerate() {
                    let mut p = positions.clone();
                    p.push(i as u32);
                    let mut s = shocks.clone();
                    s.extend(step.iter());
                    next.push((p, s));
                }
            }
            out = next;
        }
        out
    }
}

fn aims(shock: &Shock, object: &str, subject: &str) -> bool {
    shock.object == object && (shock.every || shock.subject_id == subject)
}

/// A bucketed value at `days`: linear between key tenors, flat beyond.
fn bucket(buckets: &[(f64, f64)], days: f64) -> f64 {
    match buckets {
        [] => 0.0,
        [only] => only.1,
        _ => {
            if days <= buckets[0].0 {
                return buckets[0].1;
            }
            for pair in buckets.windows(2) {
                let ((d0, v0), (d1, v1)) = (pair[0], pair[1]);
                if days <= d1 {
                    return v0 + (v1 - v0) * (days - d0) / (d1 - d0).max(1e-12);
                }
            }
            buckets[buckets.len() - 1].1
        }
    }
}

/// A point's shocks, as one move of each market object.
pub struct Applied<'a>(pub &'a [&'a Shock]);

impl Applied<'_> {
    /// A spot shocked: relative in percent, or absolute in its currency,
    /// each shock aimed at it in turn (ruling 581).
    pub fn spot(&self, subject: &str, spot: f64) -> f64 {
        self.0
            .iter()
            .filter(|s| aims(s, SPOT, subject))
            .fold(spot, |at, s| match s.form.as_str() {
                RELATIVE => at * (1.0 + s.value / 100.0),
                _ => at + s.value,
            })
    }

    /// A volatility shocked, in absolute vol points: parallel, or by expiry
    /// bucket at `days` to the option's expiry.
    pub fn vol(&self, underlying: &str, days: f64, vol: f64) -> f64 {
        let moved: f64 = self
            .0
            .iter()
            .filter(|s| aims(s, VOL_SURFACE, underlying))
            .map(|s| match s.form.as_str() {
                BUCKETED => bucket(&s.buckets, days),
                _ => s.value,
            })
            .sum();
        (vol + moved / 100.0).max(1e-6)
    }

    /// A rate shocked, in basis points: parallel, or by key tenor at `days`.
    pub fn rate(&self, curve: &str, days: f64, rate: f64) -> f64 {
        let moved: f64 = self
            .0
            .iter()
            .filter(|s| aims(s, RATE, curve))
            .map(|s| match s.form.as_str() {
                BUCKETED => bucket(&s.buckets, days),
                _ => s.value,
            })
            .sum();
        rate + moved / 10_000.0
    }

    /// The valuation time moved: minutes forward, and whether every option
    /// is moved to its own settlement time (G16).
    pub fn time(&self) -> (f64, bool) {
        let mut minutes = 0.0;
        let mut to_settlement = false;
        for s in self.0.iter().filter(|s| s.object == TIME) {
            match (s.form.as_str(), s.unit.as_str()) {
                (TO_SETTLEMENT, _) => to_settlement = true,
                (_, DAYS) => minutes += s.value * 1440.0,
                _ => minutes += s.value,
            }
        }
        (minutes, to_settlement)
    }
}

// ── From the wire (the component's side) ───────────────────────────────────

use meridian_domain::v1 as wire;

/// A tenor in days (v27's Tenor): a month a twelfth of a year, an overnight,
/// tom-next or spot-next one day.
pub fn tenor_days(tenor: &wire::Tenor) -> f64 {
    let count = f64::from(tenor.count);
    match wire::TenorUnit::try_from(tenor.unit).unwrap_or(wire::TenorUnit::Unspecified) {
        wire::TenorUnit::Week => count * 7.0,
        wire::TenorUnit::Month => count * 365.0 / 12.0,
        wire::TenorUnit::Year => count * 365.0,
        wire::TenorUnit::Day => count,
        _ => count.max(1.0),
    }
}

macro_rules! wire_decimal {
    ($value:expr) => {
        $value
            .and_then(|d| meridian_domain::exact::Exact::from_wire(d).ok())
            .map(|e| e.to_string())
    };
}

/// A wire shock as the document carries it.
pub fn shock_json(shock: &wire::Shock) -> Value {
    let target = shock.target.clone().unwrap_or_default();
    serde_json::json!({
        "object": wire::ShockObject::try_from(shock.object).unwrap_or(wire::ShockObject::Unspecified).as_str_name(),
        "subject_id": target.subject_id,
        "every": target.every,
        "form": wire::ShockForm::try_from(shock.form).unwrap_or(wire::ShockForm::Unspecified).as_str_name(),
        "unit": wire::ShockUnit::try_from(shock.unit).unwrap_or(wire::ShockUnit::Unspecified).as_str_name(),
        "value": wire_decimal!(shock.value.as_ref()),
        "buckets": shock.buckets.iter().map(|b| serde_json::json!({
            "days": tenor_days(&b.tenor.unwrap_or_default()).to_string(),
            "value": wire_decimal!(b.value.as_ref()),
        })).collect::<Vec<_>>(),
    })
}

/// A wire scenario set as the document carries it, under its row key
/// (`inline` for a job's own) and version.
pub fn set_json(row_key: &str, version: u64, set: &wire::ScenarioSet) -> Value {
    serde_json::json!({
        "row_key": row_key,
        "version": version.to_string(),
        "name": set.name,
        "axes": set.axes.iter().map(|axis| serde_json::json!({
            "name": axis.name,
            "steps": axis.steps.iter().map(|step| serde_json::json!({
                "name": step.name,
                "shocks": step.shocks.iter().map(shock_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// A saved single scenario: a set of one axis with one step.
pub fn single(scenario: &wire::Scenario) -> wire::ScenarioSet {
    wire::ScenarioSet {
        name: scenario.name.clone(),
        axes: vec![wire::ScenarioAxis {
            name: scenario.name.clone(),
            steps: vec![scenario.clone()],
        }],
    }
}

/// Why a wire scenario set cannot stand, naming the field under `at`
/// (`scenario_set`): the names a person gave, each axis's steps, and each
/// shock's form, unit, target and value (rulings 581 to 583).
pub fn wire_refused(at: &str, set: &wire::ScenarioSet) -> Option<(String, String)> {
    if set.name.trim().is_empty() {
        return Some((format!("{at}.name"), "a set is named".into()));
    }
    if set.axes.is_empty() {
        return Some((format!("{at}.axes"), "a set has one axis or more".into()));
    }
    for (a, axis) in set.axes.iter().enumerate() {
        let here = format!("{at}.axes[{a}]");
        if axis.name.trim().is_empty() {
            return Some((format!("{here}.name"), "an axis is named".into()));
        }
        if axis.steps.is_empty() {
            return Some((
                format!("{here}.steps"),
                "an axis has one step or more".into(),
            ));
        }
        for (st, step) in axis.steps.iter().enumerate() {
            let here = format!("{here}.steps[{st}]");
            if step.name.trim().is_empty() {
                return Some((format!("{here}.name"), "a scenario is named".into()));
            }
            for (sh, shock) in step.shocks.iter().enumerate() {
                let here = format!("{here}.shocks[{sh}]");
                let read = shock_json(shock);
                let bucketed = read["form"] == BUCKETED;
                let settles = read["form"] == TO_SETTLEMENT;
                if !bucketed && !settles && shock.value.is_none() {
                    return Some((format!("{here}.value"), "a shock states its value".into()));
                }
                if !bucketed && !shock.buckets.is_empty() {
                    return Some((
                        format!("{here}.buckets"),
                        "only a bucketed shock carries buckets".into(),
                    ));
                }
                if shock
                    .buckets
                    .iter()
                    .any(|b| b.tenor.is_none() || b.value.is_none())
                {
                    return Some((
                        format!("{here}.buckets"),
                        "a bucket states its tenor and value".into(),
                    ));
                }
                let parsed = Shock {
                    object: read["object"].as_str().unwrap_or_default().to_string(),
                    subject_id: read["subject_id"].as_str().unwrap_or_default().to_string(),
                    every: read["every"].as_bool().unwrap_or(false),
                    form: read["form"].as_str().unwrap_or_default().to_string(),
                    unit: read["unit"].as_str().unwrap_or_default().to_string(),
                    value: 0.0,
                    buckets: if bucketed {
                        vec![(0.0, 0.0)]
                    } else {
                        Vec::new()
                    },
                };
                if let Some(why) = refused(&parsed) {
                    let field = why.split(':').next().unwrap_or("object").to_string();
                    return Some((format!("{here}.{field}"), why));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shock(object: &str, form: &str, unit: &str, value: &str) -> Value {
        json!({"object": object, "subject_id": "", "every": true, "form": form, "unit": unit,
               "value": value, "buckets": []})
    }

    #[test]
    fn a_set_generates_the_product_of_its_axes_each_keyed_by_its_positions() {
        let inputs = json!({"scenario": {"row_key": "inline", "version": "0", "axes": [
            {"name": "spot", "steps": [
                {"name": "-10%", "shocks": [shock(SPOT, RELATIVE, PERCENT, "-10")]},
                {"name": "0", "shocks": []},
                {"name": "+10%", "shocks": [shock(SPOT, RELATIVE, PERCENT, "10")]}]},
            {"name": "date", "steps": [
                {"name": "now", "shocks": []},
                {"name": "+7d", "shocks": [shock(TIME, ABSOLUTE, DAYS, "7")]}]}]}});
        let set = Set::read(&inputs).unwrap().unwrap();
        let points = set.points();
        assert_eq!(points.len(), 6);
        assert_eq!(points[0].0, vec![0, 0]);
        assert_eq!(points[5].0, vec![2, 1]);
        let applied = Applied(&points[5].1);
        assert!((applied.spot("INS-X", 100.0) - 110.0).abs() < 1e-12);
        assert_eq!(applied.time(), (7.0 * 1440.0, false));
        assert!((Applied(&points[0].1).spot("INS-X", 100.0) - 90.0).abs() < 1e-12);
    }

    #[test]
    fn a_shock_states_its_form_and_unit_and_its_target() {
        let mut bad = shock(SPOT, PARALLEL, PERCENT, "1");
        let refused =
            |v: &Value| Set::read(&json!({"scenario": {"axes": [{"steps": [{"shocks": [v]}]}]}}));
        assert!(refused(&bad).unwrap_err().0.contains("form"));
        bad = shock(VOL_SURFACE, PARALLEL, PERCENT, "1");
        assert!(refused(&bad).unwrap_err().0.contains("unit"));
        bad = shock(RATE, PARALLEL, BASIS_POINTS, "25");
        bad["every"] = json!(false);
        assert!(refused(&bad).unwrap_err().0.contains("target"));
        let ok = shock(TIME, TO_SETTLEMENT, "SHOCK_UNIT_UNSPECIFIED", "0");
        assert!(refused(&ok).is_ok());
    }

    #[test]
    fn a_bucketed_shock_is_linear_between_its_tenors_and_flat_beyond() {
        let b = [(30.0, 2.0), (90.0, 1.0)];
        assert_eq!(bucket(&b, 10.0), 2.0);
        assert!((bucket(&b, 60.0) - 1.5).abs() < 1e-12);
        assert_eq!(bucket(&b, 400.0), 1.0);
        let s = Shock {
            object: VOL_SURFACE.into(),
            subject_id: String::new(),
            every: true,
            form: BUCKETED.into(),
            unit: VOL_POINTS.into(),
            value: 0.0,
            buckets: b.to_vec(),
        };
        let held = [&s];
        assert!((Applied(&held).vol("INS-X", 60.0, 0.20) - 0.215).abs() < 1e-12);
    }
}
