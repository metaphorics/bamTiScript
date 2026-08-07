use std::sync::Arc;

use bamts_compiler::{
    checker::{
        ProgramCheckInput, ProgramCheckOptions, ResolvedModuleEdge, check_program_with_options,
        check_with_lints,
    },
    diagnostic::Recovered,
    lint::{
        LintLevel, LintOverride, LintProfile, LintTable, RULES, RuleDefinition, RuleExampleCase,
        RuleExampleSource, rule_reference,
    },
    parser, rules, scanner,
    source::{ScriptKind, SourceId, SourceText},
    syntax::{SourceFile, Statement},
};

const REFERENCE: &str = include_str!("../RULES.md");

#[test]
fn every_registered_rule_has_complete_metadata_and_current_reference() {
    assert_eq!(RULES.len(), 88, "the adopted catalog has exactly 88 rules");
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
    assert!(!rule.code().is_empty(), "rule has an empty code");
    assert!(!rule.slug().is_empty(), "{} has an empty slug", rule.code());
    assert!(
        !rule.group().slug().is_empty(),
        "{} has an empty group slug",
        rule.code()
    );
    assert!(
        !rule.rationale().is_empty(),
        "{} has an empty rationale",
        rule.code()
    );
    assert!(
        !rule.sound_alternative().is_empty(),
        "{} has an empty sound alternative",
        rule.code()
    );
    assert_eq!(
        rule.silence_flag(),
        format!("-A {}", rule.slug()),
        "{} has an inconsistent silence flag",
        rule.code()
    );
    assert_case_nonempty(rule.examples().trigger(), rule, "trigger");
    assert_case_nonempty(rule.examples().clean(), rule, "clean");
    if let (RuleExampleCase::CompilerOptions(trigger), RuleExampleCase::CompilerOptions(clean)) =
        (rule.examples().trigger(), rule.examples().clean())
    {
        assert_ne!(
            trigger,
            clean,
            "{} uses identical trigger and clean compiler options",
            rule.code()
        );
    }
}

fn assert_case_nonempty(case: RuleExampleCase, rule: &RuleDefinition, lane: &str) {
    match case {
        RuleExampleCase::Source(source) => assert!(
            !source.text().trim().is_empty(),
            "{} has an empty {lane} source example",
            rule.code()
        ),
        RuleExampleCase::Program(sources) => {
            assert!(
                !sources.is_empty(),
                "{} has an empty {lane} program example",
                rule.code()
            );
            assert!(
                sources
                    .iter()
                    .all(|source| !source.text().trim().is_empty()),
                "{} has an empty source in its {lane} program example",
                rule.code()
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

#[test]
fn debugger_is_reported_only_when_a_profile_or_override_enables_it() {
    let default_codes = run(
        RuleExampleCase::Source(RuleExampleSource::new(ScriptKind::TypeScript, "debugger;")),
        &LintTable::new(LintProfile::Default),
    );
    assert!(
        !default_codes.iter().any(|code| code == "BAMTS-W087"),
        "default profile must leave no-debugger disabled: {default_codes:?}"
    );

    let pedantic_codes = run(
        RuleExampleCase::Source(RuleExampleSource::new(ScriptKind::TypeScript, "debugger;")),
        &LintTable::new(LintProfile::Pedantic),
    );
    assert!(
        pedantic_codes.iter().any(|code| code == "BAMTS-W087"),
        "pedantic profile must enable no-debugger: {pedantic_codes:?}"
    );

    let debugger = RULES
        .iter()
        .find(|rule| rule.code() == "BAMTS-W087")
        .expect("W087 is registered");
    let mut levels = LintTable::new(LintProfile::Default);
    levels
        .apply_cli([LintOverride::rule(
            debugger.id(),
            LintLevel::Warn,
            "test override",
        )])
        .expect("a rule override can enable no-debugger");
    let override_codes = run(
        RuleExampleCase::Source(RuleExampleSource::new(ScriptKind::TypeScript, "debugger;")),
        &levels,
    );
    assert!(
        override_codes.iter().any(|code| code == "BAMTS-W087"),
        "a rule override must enable no-debugger: {override_codes:?}"
    );
}

#[test]
fn with_reports_javascript_compatibility_and_visits_its_children() {
    let debugger = RULES
        .iter()
        .find(|rule| rule.code() == "BAMTS-W087")
        .expect("W087 is registered");
    let mut levels = LintTable::new(LintProfile::Default);
    levels
        .apply_cli([LintOverride::rule(
            debugger.id(),
            LintLevel::Warn,
            "test nested body traversal",
        )])
        .expect("a rule override can enable nested no-debugger");

    let parsed = parse(
        0,
        RuleExampleSource::new(ScriptKind::JavaScript, "with (value) { debugger; }"),
    );
    let codes = rules::analyze(parsed.product(), &levels)
        .iter()
        .map(|diagnostic| diagnostic.code().as_str().to_owned())
        .collect::<Vec<_>>();
    for code in ["BAMTS-W087", "BAMTS-W088"] {
        assert!(
            codes.iter().any(|actual| actual == code),
            "{code} must be reported from the with statement: {codes:?}"
        );
    }
}

#[test]
fn commonjs_named_export_examples_resolve_the_wrapper_environment() {
    let rule = RULES
        .iter()
        .find(|rule| rule.code() == "BAMTS-W086")
        .expect("W086 is registered");
    let levels = only_rule_enabled(rule);
    let RuleExampleCase::Program(trigger) = rule.examples().trigger() else {
        panic!("W086 trigger is a program example");
    };
    let trigger_codes = run_program_with_options(trigger, &levels, ProgramCheckOptions::commonjs());
    assert_eq!(trigger_codes, ["BAMTS-W086"]);

    let RuleExampleCase::Program(clean) = rule.examples().clean() else {
        panic!("W086 clean case is a program example");
    };
    let clean_codes = run_program_with_options(clean, &levels, ProgramCheckOptions::commonjs());
    assert!(
        clean_codes.is_empty(),
        "clean CommonJS example: {clean_codes:?}"
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
    run_program_with_options(sources, levels, ProgramCheckOptions::standard())
}

fn run_program_with_options(
    sources: &[RuleExampleSource],
    levels: &LintTable,
    options: ProgramCheckOptions,
) -> Vec<String> {
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
    check_program_with_options(
        ProgramCheckInput {
            files: &files,
            edges: &edges,
        },
        levels,
        options,
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
        Arc::new(SourceText::new(source.text()).expect("test source fits the per-file budget")),
    ))
}
