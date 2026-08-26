//! Pre-registered stage-1 regression guard (E5.4).
//!
//! Reads the landed `bench/compiler-rules.toml` plus the registered baseline
//! and current scorecard JSON, then evaluates the `stage1.pre-registered-regression`
//! rule.  It compares condition identity first, then uses the configured median
//! and the baseline tolerance to decide pass/fail.  Missing or malformed inputs
//! fail closed.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pass { current_median: f64, bound: f64 },
    Fail(String),
}

#[derive(Debug)]
pub struct GuardError(String);

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GuardError {}

impl From<toml::de::Error> for GuardError {
    fn from(error: toml::de::Error) -> Self {
        GuardError(format!("TOML parse error: {error}"))
    }
}

impl From<serde_json::Error> for GuardError {
    fn from(error: serde_json::Error) -> Self {
        GuardError(format!("JSON parse error: {error}"))
    }
}

impl From<std::io::Error> for GuardError {
    fn from(error: std::io::Error) -> Self {
        GuardError(format!("I/O error: {error}"))
    }
}

pub type Result<T, E = GuardError> = std::result::Result<T, E>;

#[derive(Debug, Deserialize)]
struct RulesDoc {
    #[serde(rename = "conditions")]
    _conditions: Conditions,
    measurement: Measurement,
    rule: HashMap<String, Rule>,
}

#[derive(Debug, Deserialize)]
struct Conditions {
    #[serde(rename = "required_governors")]
    _required_governors: Vec<String>,
    #[serde(rename = "require_swap_disabled")]
    _require_swap_disabled: bool,
    #[serde(rename = "require_pinned_affinity")]
    _require_pinned_affinity: bool,
    #[serde(rename = "forbid_full_machine_affinity")]
    _forbid_full_machine_affinity: bool,
    #[serde(rename = "recapture_after_run")]
    _recapture_after_run: bool,
}

#[derive(Debug, Deserialize)]
struct Measurement {
    #[serde(rename = "statistic")]
    _statistic: String,
    samples: usize,
    #[serde(rename = "warmup")]
    _warmup: usize,
}

#[derive(Debug, Deserialize)]
struct Rule {
    #[serde(rename = "kind")]
    _kind: String,
    #[serde(rename = "unit")]
    _unit: String,
    direction: String,
    statistic: String,
    #[serde(rename = "workload")]
    _workload: String,
    #[serde(rename = "argv")]
    _argv: Vec<String>,
    metric_key: String,
    #[serde(rename = "baseline")]
    _baseline: String,
    tolerance_source: String,
    #[serde(default, rename = "requires")]
    _requires: String,
}

#[derive(Debug, Deserialize)]
struct Baseline {
    conditions: Value,
    value: f64,
    tolerance: f64,
}

#[derive(Debug, Deserialize)]
struct Scorecard {
    conditions: Value,
    metrics: HashMap<String, Vec<f64>>,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Evaluate the `stage1.pre-registered-regression` guard from in-memory content.
pub fn evaluate_stage1_guard(
    rules_toml: &str,
    baseline_json: &str,
    scorecard_json: &str,
) -> Result<Verdict> {
    let rules: RulesDoc = toml::from_str(rules_toml)?;

    let rule = rules
        .rule
        .get("stage1.pre-registered-regression")
        .ok_or_else(|| GuardError("missing rule `stage1.pre-registered-regression`".into()))?;

    if rule.statistic != "median" {
        return Err(GuardError(format!(
            "unsupported rule statistic `{}`",
            rule.statistic
        )));
    }
    if rule.tolerance_source != "baseline" {
        return Err(GuardError(format!(
            "unsupported tolerance_source `{}`",
            rule.tolerance_source
        )));
    }

    let baseline: Baseline = serde_json::from_str(baseline_json)?;
    let scorecard: Scorecard = serde_json::from_str(scorecard_json)?;

    // Condition identity first.
    if baseline.conditions != scorecard.conditions {
        return Ok(Verdict::Fail(
            "baseline and scorecard conditions are not identical".into(),
        ));
    }

    // Missing metric fails closed.
    let samples = scorecard
        .metrics
        .get(&rule.metric_key)
        .ok_or_else(|| GuardError(format!("missing metric `{}`", rule.metric_key)))?;

    if samples.is_empty() {
        return Ok(Verdict::Fail("no samples for metric".into()));
    }
    if samples.len() != rules.measurement.samples {
        return Ok(Verdict::Fail(format!(
            "expected {} samples, got {}",
            rules.measurement.samples,
            samples.len()
        )));
    }

    let mut values = samples.clone();
    let current_median = median(&mut values);
    if current_median.is_nan() {
        return Ok(Verdict::Fail("computed median is NaN".into()));
    }

    let upper_bound = baseline.value + baseline.tolerance;

    match rule.direction.as_str() {
        "lower-is-better" => {
            if current_median > upper_bound {
                Ok(Verdict::Fail(format!(
                    "median {current_median} exceeds upper bound {upper_bound}"
                )))
            } else {
                Ok(Verdict::Pass {
                    current_median,
                    bound: upper_bound,
                })
            }
        }
        other => Err(GuardError(format!("unsupported direction `{other}`"))),
    }
}

/// Evaluate the guard from file paths.
pub fn evaluate_stage1_guard_from_paths(
    rules_toml_path: &Path,
    baseline_json_path: &Path,
    scorecard_json_path: &Path,
) -> Result<Verdict> {
    let rules = fs::read_to_string(rules_toml_path)?;
    let baseline = fs::read_to_string(baseline_json_path)?;
    let scorecard = fs::read_to_string(scorecard_json_path)?;
    evaluate_stage1_guard(&rules, &baseline, &scorecard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> &'static str {
        r#"
[conditions]
required_governors = ["performance"]
require_swap_disabled = true
require_pinned_affinity = true
forbid_full_machine_affinity = true
recapture_after_run = true

[measurement]
statistic = "median"
samples = 9
warmup = 2

[rule."stage1.pre-registered-regression"]
kind = "guard"
unit = "nanoseconds"
direction = "lower-is-better"
statistic = "median"
workload = "corpus/cases/valita.ts"
argv = ["-r", "--jit", "--json"]
metric_key = "stage1.total_ns"
baseline = "bench/baselines/stage1.json"
tolerance_source = "baseline"
requires = "a registered stage-1 baseline recorded on the measuring host"
"#
    }

    fn baseline(value: f64, tolerance: f64) -> String {
        format!(
            r#"{{
  "conditions": {{
    "governor": "performance",
    "cpu_affinity": "0-19",
    "swap_total_kib": 0
  }},
  "value": {value},
  "tolerance": {tolerance}
}}"#
        )
    }

    fn scorecard(samples: &[f64]) -> String {
        let arr = samples
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"{{
  "conditions": {{
    "governor": "performance",
    "cpu_affinity": "0-19",
    "swap_total_kib": 0
  }},
  "metrics": {{
    "stage1.total_ns": [{arr}]
  }}
}}"#
        )
    }

    #[test]
    fn median_odd() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn median_even() {
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn pass_when_within_tolerance() {
        let result =
            evaluate_stage1_guard(rules(), &baseline(100.0, 10.0), &scorecard(&[100.0; 9]));
        assert!(matches!(result, Ok(Verdict::Pass { .. })), "{result:?}");
    }

    #[test]
    fn fail_on_regression() {
        let result =
            evaluate_stage1_guard(rules(), &baseline(100.0, 10.0), &scorecard(&[120.0; 9]));
        assert!(matches!(result, Ok(Verdict::Fail(_))), "{result:?}");
    }

    #[test]
    fn fail_on_condition_mismatch() {
        let s = scorecard(&[100.0; 9]).replace("0-19", "0-15");
        let result = evaluate_stage1_guard(rules(), &baseline(100.0, 10.0), &s);
        assert!(matches!(result, Ok(Verdict::Fail(_))), "{result:?}");
    }

    #[test]
    fn fail_on_missing_metric() {
        let s = r#"{
  "conditions": { "governor": "performance", "cpu_affinity": "0-19", "swap_total_kib": 0 },
  "metrics": {}
}"#;
        let result = evaluate_stage1_guard(rules(), &baseline(100.0, 10.0), s);
        assert!(
            matches!(result, Err(_) | Ok(Verdict::Fail(_))),
            "{result:?}"
        );
    }

    #[test]
    fn fail_on_wrong_sample_count() {
        let result =
            evaluate_stage1_guard(rules(), &baseline(100.0, 10.0), &scorecard(&[100.0; 5]));
        assert!(matches!(result, Ok(Verdict::Fail(_))), "{result:?}");
    }

    #[test]
    fn reject_unsupported_statistic() {
        let toml = rules().replace("statistic = \"median\"", "statistic = \"mean\"");
        let result = evaluate_stage1_guard(&toml, &baseline(100.0, 10.0), &scorecard(&[100.0; 9]));
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn reject_unsupported_direction() {
        let toml = rules().replace("lower-is-better", "higher-is-better");
        let result = evaluate_stage1_guard(&toml, &baseline(100.0, 10.0), &scorecard(&[100.0; 9]));
        assert!(result.is_err(), "{result:?}");
    }
}
