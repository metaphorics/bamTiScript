use std::path::Path;

use bamts_compiler::lint::{rule_reference, RuleDefinition, RULES};

const REFERENCE: &str = include_str!("../RULES.md");
const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rules");

#[test]
fn every_registered_rule_has_complete_metadata_and_current_reference() {
    assert_eq!(RULES.len(), 86, "the adopted catalog has exactly 86 rules");
    assert_eq!(REFERENCE, rule_reference(), "regenerate RULES.md from RULES");

    for rule in &RULES {
        assert_complete_metadata(rule);
    }
}

fn assert_complete_metadata(rule: &RuleDefinition) {
    assert!(!rule.code().is_empty());
    assert!(!rule.slug().is_empty());
    assert!(!rule.group().slug().is_empty());
    assert!(!rule.rationale().is_empty());
    assert!(!rule.sound_alternative().is_empty());
    assert_eq!(rule.silence_flag(), format!("-A {}", rule.slug()));
}

/// Task 16 owns semantic fixtures. Remove `ignore` only after it supplies both
/// minimal cases for every implemented rule; this is deliberately driven by RULES.
#[test]
#[ignore = "Task 16 must supply triggering and non-triggering fixtures for every rule"]
fn every_registered_rule_has_trigger_and_non_trigger_fixtures() {
    for rule in &RULES {
        let directory = Path::new(FIXTURE_ROOT).join(rule.slug());
        assert!(
            directory.join("trigger.ts").is_file(),
            "{} needs a triggering fixture",
            rule.code()
        );
        assert!(
            directory.join("non-trigger.ts").is_file(),
            "{} needs a non-triggering fixture",
            rule.code()
        );
    }
}
