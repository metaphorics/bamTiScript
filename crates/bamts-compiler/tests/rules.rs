use std::sync::Arc;

use bamts_compiler::{
    checker::{ProgramCheckInput, ResolvedModuleEdge, check_program, check_with_lints},
    diagnostic::Recovered,
    lint::{
        LintLevel, LintOverride, LintProfile, LintTable, RULES, RuleDefinition, RuleExampleCase,
        RuleExampleSource, rule_reference,
    },
    parser, rules, scanner,
    source::{SourceId, SourceText},
    syntax::{SourceFile, Statement},
};

const REFERENCE: &str = include_str!("../RULES.md");

#[test]
fn every_registered_rule_has_complete_metadata_and_current_reference() {
    assert_eq!(RULES.len(), 86, "the adopted catalog has exactly 86 rules");
    assert_eq!(
        REFERENCE,
        rule_reference(),
        "regenerate RULES.md from RULES"
    );

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
    assert_case_nonempty(rule.examples().trigger(), rule);
    assert_case_nonempty(rule.examples().clean(), rule);
}

fn assert_case_nonempty(case: RuleExampleCase, rule: &RuleDefinition) {
    match case {
        RuleExampleCase::Source(source) => assert!(!source.text().trim().is_empty()),
        RuleExampleCase::Program(sources) => {
            assert!(
                !sources.is_empty(),
                "{} has an empty program example",
                rule.code()
            );
            assert!(
                sources
                    .iter()
                    .all(|source| !source.text().trim().is_empty())
            );
        }
        RuleExampleCase::CompilerOptions(_) => {}
    }
}

#[test]
fn every_registered_rule_triggers_and_has_a_clean_counterexample() {
    let mut failures = Vec::new();
    for rule in &RULES {
        let levels = only_rule_enabled(rule);
        let trigger_codes = run(rule.examples().trigger(), &levels);
        if trigger_codes.iter().all(|code| code != rule.code()) {
            failures.push(format!(
                "{} ({}) did not trigger; diagnostics: {trigger_codes:?}",
                rule.code(),
                rule.slug(),
            ));
        }

        let clean_codes = run(rule.examples().clean(), &levels);
        if clean_codes.iter().any(|code| code == rule.code()) {
            failures.push(format!(
                "{} ({}) fired for its clean example; diagnostics: {clean_codes:?}",
                rule.code(),
                rule.slug(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "rule contract failures:\n{}",
        failures.join("\n")
    );
}

fn only_rule_enabled(rule: &'static RuleDefinition) -> LintTable {
    let mut levels = LintTable::new(LintProfile::Default);
    levels
        .apply_cli(RULES.iter().map(|candidate| {
            LintOverride::rule(candidate.id(), LintLevel::Allow, "rule contract isolation")
        }))
        .expect("allowing catalog rules cannot lower a forbid lock");
    levels
        .apply_cli([LintOverride::rule(
            rule.id(),
            LintLevel::Warn,
            "rule contract target",
        )])
        .expect("examples can enable their target rule");
    levels
}

fn run(case: RuleExampleCase, levels: &LintTable) -> Vec<String> {
    match case {
        RuleExampleCase::Source(source) => check_with_lints(&parse(0, source), levels)
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().as_str().to_owned())
            .collect(),
        RuleExampleCase::Program(sources) => run_program(sources, levels),
        RuleExampleCase::CompilerOptions(options) => {
            rules::analyze_compiler_options(options, levels, SourceId::new(0))
                .iter()
                .map(|diagnostic| diagnostic.code().as_str().to_owned())
                .collect()
        }
    }
}

fn run_program(sources: &[RuleExampleSource], levels: &LintTable) -> Vec<String> {
    let files = sources
        .iter()
        .enumerate()
        .map(|(index, source)| parse(index, *source))
        .collect::<Vec<_>>();
    let edges = sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            let target = source.resolves_to()?;
            let file = files[index].product();
            let specifier = file.statements().iter().find_map(|statement| {
                matches!(
                    statement.data(),
                    Statement::Import(_) | Statement::Export(_)
                )
                .then_some(statement.id())
            })?;
            Some(ResolvedModuleEdge {
                from: SourceId::new(u32::try_from(index).expect("example source index fits u32")),
                specifier,
                to: SourceId::new(u32::try_from(target).expect("example target index fits u32")),
            })
        })
        .collect::<Vec<_>>();
    check_program(
        ProgramCheckInput {
            files: &files,
            edges: &edges,
        },
        levels,
    )
    .diagnostics()
    .iter()
    .map(|diagnostic| diagnostic.code().as_str().to_owned())
    .collect()
}

fn parse(index: usize, source: RuleExampleSource) -> Recovered<SourceFile> {
    parser::parse(scanner::scan(
        SourceId::new(u32::try_from(index).expect("example source index fits u32")),
        source.script_kind(),
        Arc::new(SourceText::new(source.text())),
    ))
}
