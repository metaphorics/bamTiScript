use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer};

pub const RULES_SCHEMA: &str = "bamti.compiler-rules/v1";
pub const RULE_IDS: [&str; 9] = [
    "jit.compile-cost",
    "jit.payback",
    "jit.queue-tail-latency",
    "stage0.aot-no-jit-allocator",
    "stage0.correctness",
    "stage0.event-completeness",
    "stage0.integrity",
    "stage0.no-rwx",
    "stage1.pre-registered-regression",
];
pub const JIT_RULE_IDS: [&str; 3] = ["jit.compile-cost", "jit.payback", "jit.queue-tail-latency"];
pub const STAGE0_RULE_IDS: [&str; 5] = [
    "stage0.aot-no-jit-allocator",
    "stage0.correctness",
    "stage0.event-completeness",
    "stage0.integrity",
    "stage0.no-rwx",
];
pub const STAGE1_RULE_IDS: [&str; 1] = ["stage1.pre-registered-regression"];

const CATALOG_SCHEMA: &str = "bamti.catalog-inputs/v1";
const MEDIAN: &str = "median";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerRules {
    pub schema: String,
    pub conditions: Conditions,
    pub measurement: Measurement,
    pub rule: BTreeMap<String, Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    pub required_governors: Vec<String>,
    pub require_swap_disabled: bool,
    pub require_pinned_affinity: bool,
    pub forbid_full_machine_affinity: bool,
    pub recapture_after_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub statistic: String,
    pub samples: usize,
    pub warmup: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Rule {
    Duration {
        unit: String,
        direction: String,
        workload: String,
        argv: Vec<String>,
        metric_key: String,
        requires: String,
    },
    Count {
        unit: String,
        direction: String,
        workload: String,
        argv: Vec<String>,
        metric_key: String,
        requires: String,
    },
    Predicate {
        decided_by: String,
        workload: String,
        argv: Vec<String>,
        baseline_argv: Option<Vec<String>>,
        metric_key: Option<String>,
        requires: String,
    },
    Guard {
        unit: String,
        direction: String,
        statistic: String,
        workload: String,
        argv: Vec<String>,
        metric_key: String,
        baseline: String,
        tolerance_source: String,
        requires: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Duration,
    Count,
    Predicate,
    Guard,
}

impl Rule {
    pub const fn kind(&self) -> RuleKind {
        match self {
            Self::Duration { .. } => RuleKind::Duration,
            Self::Count { .. } => RuleKind::Count,
            Self::Predicate { .. } => RuleKind::Predicate,
            Self::Guard { .. } => RuleKind::Guard,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    RulesToml(String),
    CatalogJson(String),
    RulesSchema {
        actual: String,
    },
    CatalogSchema {
        actual: String,
    },
    BenchmarkCatalogCount {
        actual: usize,
    },
    RuleInventory {
        actual: Vec<String>,
    },
    CatalogInventory {
        actual: Vec<String>,
    },
    WrongKind {
        rule: String,
        expected: RuleKind,
        actual: RuleKind,
    },
    InvalidVocabulary {
        rule: Option<String>,
        field: &'static str,
        value: String,
    },
    InvalidSampleCount {
        samples: usize,
    },
    MissingField {
        rule: String,
        field: &'static str,
    },
    UnexpectedField {
        rule: String,
        field: &'static str,
    },
    NaNSample,
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RulesToml(error) => write!(formatter, "invalid compiler rules TOML: {error}"),
            Self::CatalogJson(error) => write!(formatter, "invalid catalog JSON: {error}"),
            Self::RulesSchema { actual } => write!(
                formatter,
                "expected compiler rules schema `{RULES_SCHEMA}`, found `{actual}`"
            ),
            Self::CatalogSchema { actual } => write!(
                formatter,
                "expected catalog schema `{CATALOG_SCHEMA}`, found `{actual}`"
            ),
            Self::BenchmarkCatalogCount { actual } => write!(
                formatter,
                "expected exactly one benchmarks catalog, found {actual}"
            ),
            Self::RuleInventory { actual } => {
                write!(
                    formatter,
                    "compiler rule inventory is not closed: {actual:?}"
                )
            }
            Self::CatalogInventory { actual } => {
                write!(
                    formatter,
                    "benchmark catalog inventory is not closed: {actual:?}"
                )
            }
            Self::WrongKind {
                rule,
                expected,
                actual,
            } => write!(
                formatter,
                "rule `{rule}` has kind {actual:?}, expected {expected:?}"
            ),
            Self::InvalidVocabulary { rule, field, value } => {
                if let Some(rule) = rule {
                    write!(formatter, "rule `{rule}` has invalid {field} `{value}`")
                } else {
                    write!(formatter, "invalid {field} `{value}`")
                }
            }
            Self::InvalidSampleCount { samples } => write!(
                formatter,
                "median requires a nonzero odd sample count, found {samples}"
            ),
            Self::MissingField { rule, field } => {
                write!(formatter, "rule `{rule}` requires nonempty field `{field}`")
            }
            Self::UnexpectedField { rule, field } => {
                write!(formatter, "rule `{rule}` must not define field `{field}`")
            }
            Self::NaNSample => formatter.write_str("median samples must not contain NaN"),
        }
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogInputs {
    schema: String,
    catalogs: Vec<CatalogInput>,
}

#[derive(Debug)]
struct CatalogInput {
    extractor: Option<ClosedPlanInventoryExtractor>,
    id: String,
    identifiers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedPlanInventoryExtractor {
    kind: String,
    section: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogInputWire {
    extractor: serde_json::Value,
    id: String,
    identifiers: Vec<String>,
}

impl<'de> Deserialize<'de> for CatalogInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CatalogInputWire::deserialize(deserializer)?;
        let extractor = if wire.id == "benchmarks" {
            Some(serde_json::from_value(wire.extractor).map_err(serde::de::Error::custom)?)
        } else {
            None
        };
        Ok(Self {
            extractor,
            id: wire.id,
            identifiers: wire.identifiers,
        })
    }
}

pub fn parse_and_validate(
    rules_toml: &str,
    catalog_json: &str,
) -> Result<CompilerRules, ContractError> {
    let rules: CompilerRules =
        toml::from_str(rules_toml).map_err(|error| ContractError::RulesToml(error.to_string()))?;
    let catalog: CatalogInputs = serde_json::from_str(catalog_json)
        .map_err(|error| ContractError::CatalogJson(error.to_string()))?;

    validate_rules(&rules)?;
    validate_catalog(&catalog)?;
    Ok(rules)
}

fn validate_rules(rules: &CompilerRules) -> Result<(), ContractError> {
    if rules.schema != RULES_SCHEMA {
        return Err(ContractError::RulesSchema {
            actual: rules.schema.clone(),
        });
    }

    let actual = rules.rule.keys().cloned().collect::<Vec<_>>();
    if actual.as_slice() != RULE_IDS {
        return Err(ContractError::RuleInventory { actual });
    }
    if rules.measurement.statistic != MEDIAN {
        return Err(invalid_vocabulary(
            None,
            "measurement statistic",
            &rules.measurement.statistic,
        ));
    }
    validate_sample_count(rules.measurement.samples)?;

    for id in RULE_IDS {
        let rule = &rules.rule[id];
        let expected = registered_kind(id);
        if rule.kind() != expected {
            return Err(ContractError::WrongKind {
                rule: id.to_owned(),
                expected,
                actual: rule.kind(),
            });
        }
        validate_rule(id, rule)?;
    }
    Ok(())
}

fn validate_catalog(catalog: &CatalogInputs) -> Result<(), ContractError> {
    if catalog.schema != CATALOG_SCHEMA {
        return Err(ContractError::CatalogSchema {
            actual: catalog.schema.clone(),
        });
    }

    let benchmarks = catalog
        .catalogs
        .iter()
        .filter(|entry| entry.id == "benchmarks")
        .collect::<Vec<_>>();
    if benchmarks.len() != 1 {
        return Err(ContractError::BenchmarkCatalogCount {
            actual: benchmarks.len(),
        });
    }
    let extractor = benchmarks[0]
        .extractor
        .as_ref()
        .expect("benchmarks extractor is typed during deserialization");
    if extractor.kind != "closed-plan-inventory" {
        return Err(invalid_vocabulary(
            None,
            "benchmarks extractor kind",
            &extractor.kind,
        ));
    }
    require_nonempty("benchmarks", "extractor section", &extractor.section)?;
    let actual = benchmarks[0].identifiers.clone();
    if actual.as_slice() != RULE_IDS {
        return Err(ContractError::CatalogInventory { actual });
    }
    Ok(())
}

fn registered_kind(id: &str) -> RuleKind {
    match id {
        "jit.compile-cost" | "jit.queue-tail-latency" => RuleKind::Duration,
        "jit.payback" => RuleKind::Count,
        "stage0.aot-no-jit-allocator"
        | "stage0.correctness"
        | "stage0.event-completeness"
        | "stage0.integrity"
        | "stage0.no-rwx" => RuleKind::Predicate,
        "stage1.pre-registered-regression" => RuleKind::Guard,
        _ => unreachable!("closed rule inventory"),
    }
}

fn validate_rule(id: &str, rule: &Rule) -> Result<(), ContractError> {
    let (workload, requires, argv) = match rule {
        Rule::Duration {
            workload,
            requires,
            argv,
            ..
        }
        | Rule::Count {
            workload,
            requires,
            argv,
            ..
        }
        | Rule::Predicate {
            workload,
            requires,
            argv,
            ..
        }
        | Rule::Guard {
            workload,
            requires,
            argv,
            ..
        } => (workload, requires, argv),
    };
    require_nonempty(id, "workload", workload)?;
    require_nonempty(id, "requires", requires)?;
    if argv.is_empty() {
        return Err(ContractError::MissingField {
            rule: id.to_owned(),
            field: "argv",
        });
    }

    match rule {
        Rule::Duration {
            unit,
            direction,
            metric_key,
            ..
        }
        | Rule::Count {
            unit,
            direction,
            metric_key,
            ..
        } => {
            validate_unit(id, unit)?;
            validate_lower_is_better(id, direction)?;
            require_nonempty(id, "metric_key", metric_key)
        }
        Rule::Predicate {
            decided_by,
            baseline_argv,
            metric_key,
            ..
        } => {
            validate_decider(id, decided_by)?;
            validate_predicate_fields(id, baseline_argv.as_deref(), metric_key.as_deref())
        }
        Rule::Guard {
            unit,
            direction,
            statistic,
            metric_key,
            baseline,
            tolerance_source,
            ..
        } => {
            validate_unit(id, unit)?;
            validate_lower_is_better(id, direction)?;
            if statistic != MEDIAN {
                return Err(invalid_vocabulary(Some(id), "statistic", statistic));
            }
            if tolerance_source != "baseline" {
                return Err(invalid_vocabulary(
                    Some(id),
                    "tolerance_source",
                    tolerance_source,
                ));
            }
            require_nonempty(id, "metric_key", metric_key)?;
            require_nonempty(id, "baseline", baseline)
        }
    }
}

fn validate_unit(id: &str, unit: &str) -> Result<(), ContractError> {
    let expected = match id {
        "jit.compile-cost" | "jit.queue-tail-latency" | "stage1.pre-registered-regression" => {
            "nanoseconds"
        }
        "jit.payback" => "iterations",
        _ => unreachable!("only registered metric rules have units"),
    };
    if unit == expected {
        Ok(())
    } else {
        Err(invalid_vocabulary(Some(id), "unit", unit))
    }
}

fn validate_lower_is_better(id: &str, direction: &str) -> Result<(), ContractError> {
    if direction == "lower-is-better" {
        Ok(())
    } else {
        Err(invalid_vocabulary(Some(id), "direction", direction))
    }
}

fn validate_decider(id: &str, decider: &str) -> Result<(), ContractError> {
    let expected = match id {
        "stage0.correctness" => "differential",
        "stage0.integrity" => "reproducible-artifact",
        "stage0.no-rwx" | "stage0.aot-no-jit-allocator" => "process-maps",
        "stage0.event-completeness" => "event-stream",
        _ => unreachable!("only registered predicate rules have deciders"),
    };
    if decider == expected {
        Ok(())
    } else {
        Err(invalid_vocabulary(Some(id), "decided_by", decider))
    }
}

fn validate_predicate_fields(
    id: &str,
    baseline_argv: Option<&[String]>,
    metric_key: Option<&str>,
) -> Result<(), ContractError> {
    match id {
        "stage0.correctness" => {
            let baseline_argv = baseline_argv.ok_or_else(|| ContractError::MissingField {
                rule: id.to_owned(),
                field: "baseline_argv",
            })?;
            if baseline_argv.is_empty() {
                return Err(ContractError::MissingField {
                    rule: id.to_owned(),
                    field: "baseline_argv",
                });
            }
            reject_present(id, "metric_key", metric_key.is_some())
        }
        "stage0.event-completeness" => {
            let metric_key = metric_key.ok_or_else(|| ContractError::MissingField {
                rule: id.to_owned(),
                field: "metric_key",
            })?;
            require_nonempty(id, "metric_key", metric_key)?;
            reject_present(id, "baseline_argv", baseline_argv.is_some())
        }
        _ => {
            reject_present(id, "baseline_argv", baseline_argv.is_some())?;
            reject_present(id, "metric_key", metric_key.is_some())
        }
    }
}

fn require_nonempty(id: &str, field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        Err(ContractError::MissingField {
            rule: id.to_owned(),
            field,
        })
    } else {
        Ok(())
    }
}

fn reject_present(id: &str, field: &'static str, present: bool) -> Result<(), ContractError> {
    if present {
        Err(ContractError::UnexpectedField {
            rule: id.to_owned(),
            field,
        })
    } else {
        Ok(())
    }
}

fn invalid_vocabulary(rule: Option<&str>, field: &'static str, value: &str) -> ContractError {
    ContractError::InvalidVocabulary {
        rule: rule.map(str::to_owned),
        field,
        value: value.to_owned(),
    }
}

fn validate_sample_count(samples: usize) -> Result<(), ContractError> {
    if samples != 0 && samples % 2 == 1 {
        Ok(())
    } else {
        Err(ContractError::InvalidSampleCount { samples })
    }
}

pub fn median_u64(samples: &mut [u64]) -> Result<u64, ContractError> {
    validate_sample_count(samples.len())?;
    let middle = samples.len() / 2;
    samples.select_nth_unstable(middle);
    Ok(samples[middle])
}

pub fn median_f64(samples: &mut [f64]) -> Result<f64, ContractError> {
    validate_sample_count(samples.len())?;
    if samples.iter().any(|sample| sample.is_nan()) {
        return Err(ContractError::NaNSample);
    }
    let middle = samples.len() / 2;
    samples.select_nth_unstable_by(middle, f64::total_cmp);
    Ok(samples[middle])
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = include_str!("../../../bench/compiler-rules.toml");
    const CATALOG: &str = include_str!("../../../verification/catalog-inputs.json");

    fn rules_value() -> toml::Value {
        toml::from_str(RULES).unwrap()
    }

    fn encoded_rules(value: &toml::Value) -> String {
        toml::to_string(value).unwrap()
    }

    fn assert_rules_rejected(value: &toml::Value) {
        assert!(parse_and_validate(&encoded_rules(value), CATALOG).is_err());
    }

    fn catalog_value() -> serde_json::Value {
        serde_json::from_str(CATALOG).unwrap()
    }

    fn assert_catalog_rejected(value: &serde_json::Value) {
        assert!(parse_and_validate(RULES, &serde_json::to_string(value).unwrap()).is_err());
    }

    fn benchmark_extractor(value: &mut serde_json::Value) -> &mut serde_json::Value {
        &mut value["catalogs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["id"] == "benchmarks")
            .unwrap()["extractor"]
    }

    #[test]
    fn current_policy_and_catalog_parse_strictly() {
        let rules = parse_and_validate(RULES, CATALOG).unwrap();
        assert_eq!(
            rules.rule.keys().map(String::as_str).collect::<Vec<_>>(),
            RULE_IDS
        );
    }

    #[test]
    fn rule_inventory_rejects_missing_extra_and_renamed_ids() {
        let mut missing = rules_value();
        missing["rule"]
            .as_table_mut()
            .unwrap()
            .remove("jit.compile-cost");
        assert_rules_rejected(&missing);

        let mut extra = rules_value();
        let copied = extra["rule"]["jit.compile-cost"].clone();
        extra["rule"]
            .as_table_mut()
            .unwrap()
            .insert("jit.extra".to_owned(), copied);
        assert_rules_rejected(&extra);

        let mut renamed = rules_value();
        let rule = renamed["rule"]
            .as_table_mut()
            .unwrap()
            .remove("jit.compile-cost")
            .unwrap();
        renamed["rule"]
            .as_table_mut()
            .unwrap()
            .insert("jit.compile-renamed".to_owned(), rule);
        assert_rules_rejected(&renamed);
    }

    #[test]
    fn catalog_inventory_must_equal_the_closed_rule_inventory() {
        let mut catalog: serde_json::Value = serde_json::from_str(CATALOG).unwrap();
        let benchmarks = catalog["catalogs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["id"] == "benchmarks")
            .unwrap();
        benchmarks["identifiers"].as_array_mut().unwrap().pop();
        assert!(parse_and_validate(RULES, &serde_json::to_string(&catalog).unwrap()).is_err());
    }

    #[test]
    fn registered_rule_kind_is_enforced() {
        let mut value = rules_value();
        value["rule"]["jit.compile-cost"]["kind"] = toml::Value::String("count".to_owned());
        assert!(matches!(
            parse_and_validate(&encoded_rules(&value), CATALOG),
            Err(ContractError::WrongKind { .. })
        ));
    }

    #[test]
    fn closed_vocabularies_reject_unknown_values() {
        let mut direction = rules_value();
        direction["rule"]["jit.compile-cost"]["direction"] =
            toml::Value::String("higher-is-better".to_owned());
        assert_rules_rejected(&direction);

        let mut decider = rules_value();
        decider["rule"]["stage0.correctness"]["decided_by"] =
            toml::Value::String("oracle".to_owned());
        assert_rules_rejected(&decider);

        let mut tolerance = rules_value();
        tolerance["rule"]["stage1.pre-registered-regression"]["tolerance_source"] =
            toml::Value::String("measurement".to_owned());
        assert_rules_rejected(&tolerance);
    }

    #[test]
    fn registered_predicate_deciders_cannot_be_swapped() {
        let mut value = rules_value();
        value["rule"]["stage0.correctness"]["decided_by"] =
            toml::Value::String("reproducible-artifact".to_owned());
        value["rule"]["stage0.integrity"]["decided_by"] =
            toml::Value::String("differential".to_owned());
        assert_rules_rejected(&value);
    }

    #[test]
    fn registered_units_reject_other_valid_and_arbitrary_values() {
        for (id, unit) in [
            ("jit.compile-cost", "iterations"),
            ("jit.payback", "nanoseconds"),
            ("jit.queue-tail-latency", "cycles"),
            ("stage1.pre-registered-regression", "cycles"),
        ] {
            let mut value = rules_value();
            value["rule"][id]["unit"] = toml::Value::String(unit.to_owned());
            assert_rules_rejected(&value);
        }
    }

    #[test]
    fn benchmarks_extractor_is_a_strict_closed_plan_inventory() {
        let mut scalar = catalog_value();
        *benchmark_extractor(&mut scalar) = serde_json::json!("closed-plan-inventory");
        assert_catalog_rejected(&scalar);

        let mut missing_section = catalog_value();
        benchmark_extractor(&mut missing_section)
            .as_object_mut()
            .unwrap()
            .remove("section");
        assert_catalog_rejected(&missing_section);

        let mut extra_member = catalog_value();
        benchmark_extractor(&mut extra_member)
            .as_object_mut()
            .unwrap()
            .insert("extra".to_owned(), serde_json::json!(true));
        assert_catalog_rejected(&extra_member);

        let mut wrong_kind = catalog_value();
        benchmark_extractor(&mut wrong_kind)["kind"] = serde_json::json!("cross-product");
        assert_catalog_rejected(&wrong_kind);
    }

    #[test]
    fn global_and_guard_statistics_must_be_median() {
        let mut global = rules_value();
        global["measurement"]["statistic"] = toml::Value::String("mean".to_owned());
        assert_rules_rejected(&global);

        let mut guard = rules_value();
        guard["rule"]["stage1.pre-registered-regression"]["statistic"] =
            toml::Value::String("mean".to_owned());
        assert_rules_rejected(&guard);
    }

    #[test]
    fn policy_sample_count_must_be_nonzero_and_odd() {
        for samples in [0, 8] {
            let mut value = rules_value();
            value["measurement"]["samples"] = toml::Value::Integer(samples);
            assert_rules_rejected(&value);
        }
    }

    #[test]
    fn strict_models_reject_unknown_fields() {
        let mut value = rules_value();
        value["measurement"]
            .as_table_mut()
            .unwrap()
            .insert("unknown".to_owned(), toml::Value::Boolean(true));
        assert_rules_rejected(&value);
    }

    #[test]
    fn registered_metric_and_baseline_fields_are_required() {
        let mut metric = rules_value();
        metric["rule"]["jit.compile-cost"]
            .as_table_mut()
            .unwrap()
            .remove("metric_key");
        assert_rules_rejected(&metric);

        let mut baseline = rules_value();
        baseline["rule"]["stage1.pre-registered-regression"]
            .as_table_mut()
            .unwrap()
            .remove("baseline");
        assert_rules_rejected(&baseline);

        let mut baseline_argv = rules_value();
        baseline_argv["rule"]["stage0.correctness"]
            .as_table_mut()
            .unwrap()
            .remove("baseline_argv");
        assert_rules_rejected(&baseline_argv);
    }

    #[test]
    fn common_rule_fields_reject_empty_workload_requires_and_argv() {
        for id in [
            "jit.compile-cost",
            "jit.payback",
            "stage0.correctness",
            "stage1.pre-registered-regression",
        ] {
            for field in ["workload", "requires"] {
                let mut value = rules_value();
                value["rule"][id][field] = toml::Value::String(String::new());
                assert!(matches!(
                    parse_and_validate(&encoded_rules(&value), CATALOG),
                    Err(ContractError::MissingField {
                        field: rejected,
                        ..
                    }) if rejected == field
                ));
            }

            let mut value = rules_value();
            value["rule"][id]["argv"] = toml::Value::Array(Vec::new());
            assert!(matches!(
                parse_and_validate(&encoded_rules(&value), CATALOG),
                Err(ContractError::MissingField { field: "argv", .. })
            ));
        }
    }

    #[test]
    fn u64_medians_select_observed_members_across_odd_lengths() {
        for mut samples in [
            vec![7],
            vec![9, 1, 5],
            vec![11, 3, 7, 1, 9],
            vec![13, 1, 21, 5, 17, 9, 25, 3, 29],
        ] {
            let observed = samples.clone();
            let median = median_u64(&mut samples).unwrap();
            assert!(observed.contains(&median));
            let lower = observed.iter().filter(|sample| **sample < median).count();
            let upper = observed.iter().filter(|sample| **sample > median).count();
            assert_eq!(lower, observed.len() / 2);
            assert_eq!(upper, observed.len() / 2);
        }
    }

    #[test]
    fn f64_medians_select_observed_members_across_odd_lengths() {
        for mut samples in [
            vec![7.0],
            vec![9.0, 1.0, 5.0],
            vec![11.0, 3.0, 7.0, 1.0, 9.0],
            vec![0.0, -0.0, -1.0],
            vec![f64::INFINITY, f64::NEG_INFINITY, 0.0],
            vec![1.0, -0.0, 0.0, f64::NEG_INFINITY, f64::INFINITY],
        ] {
            let observed = samples.clone();
            let mut ranked = observed.clone();
            ranked.sort_by(f64::total_cmp);
            let expected = ranked[ranked.len() / 2];
            let median = median_f64(&mut samples).unwrap();
            assert_eq!(median.to_bits(), expected.to_bits());
            assert!(
                observed
                    .iter()
                    .any(|sample| sample.to_bits() == median.to_bits())
            );
        }
    }

    #[test]
    fn medians_reject_empty_even_and_nan_samples() {
        assert!(matches!(
            median_u64(&mut []),
            Err(ContractError::InvalidSampleCount { samples: 0 })
        ));
        assert!(matches!(
            median_u64(&mut [1, 2]),
            Err(ContractError::InvalidSampleCount { samples: 2 })
        ));
        assert!(matches!(
            median_f64(&mut [1.0, 2.0]),
            Err(ContractError::InvalidSampleCount { samples: 2 })
        ));
        assert!(matches!(
            median_f64(&mut [1.0, f64::NAN, 2.0]),
            Err(ContractError::NaNSample)
        ));
    }
}
