use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, Write},
    path::Path,
    process,
};

use bamts_verification::{
    ErrorCode, Gate, GateReport, Result, VerificationError,
    formal_bridge::{
        LaneStatus, audit_formal_artifacts, qualify_replay_canary, ready_lane_records,
        regenerate_canonical_fixture, run_replay_canary_child,
    },
    formal_gates::audit_formal_gates,
    ledger::verify_ledger_g0,
    workspace_guard::audit_workspace,
};

enum Command {
    LedgerVerify,
    FormalAudit(Vec<Gate>),
    RegenerateFixtures,
    ReplayCanary { faulty: bool },
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(io::stderr().lock(), "{error}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = parse_command(env::args_os())?;
    let root = env::current_dir().map_err(|error| {
        VerificationError::new(
            ErrorCode::Io,
            format!("cannot determine current directory: {error}"),
        )
    })?;

    match command {
        Command::LedgerVerify => verify_g0(&root),
        Command::FormalAudit(gates) => audit_formal(&root, &gates),
        Command::RegenerateFixtures => regenerate_fixtures(&root),
        Command::ReplayCanary { faulty } => run_replay_canary_child(&root, faulty),
    }
}

fn parse_command(mut arguments: impl Iterator<Item = OsString>) -> Result<Command> {
    let _ = arguments.next();
    let command = required_argument(&mut arguments, "command")?;

    match command.as_str() {
        "ledger" => parse_ledger_command(&mut arguments),
        "formal" => parse_formal_command(&mut arguments),
        "--formal-canary-replay" => {
            reject_extra_arguments(&mut arguments, &[])?;
            Ok(Command::ReplayCanary { faulty: false })
        }
        "--formal-canary-replay-faulty" => {
            reject_extra_arguments(&mut arguments, &[])?;
            Ok(Command::ReplayCanary { faulty: true })
        }
        _ if command.starts_with("--") => Err(usage(format!("unknown flag `{command}`"))),
        _ => Err(usage(format!("unknown command `{command}`"))),
    }
}

fn parse_ledger_command(arguments: &mut impl Iterator<Item = OsString>) -> Result<Command> {
    expect_literal(arguments, "verify", "ledger subcommand")?;
    expect_flag(arguments, "--gate")?;
    let value = flag_value(arguments, "--gate")?;
    let gate = Gate::parse(&value)?;
    if gate != Gate::G0 {
        return Err(usage(format!(
            "ledger verify requires `--gate G0`, found `{value}`"
        )));
    }
    reject_extra_arguments(arguments, &["--gate"])?;
    Ok(Command::LedgerVerify)
}

fn parse_formal_command(arguments: &mut impl Iterator<Item = OsString>) -> Result<Command> {
    let subcommand = required_argument(arguments, "formal subcommand")?;
    match subcommand.as_str() {
        "audit" => {
            expect_flag(arguments, "--gates")?;
            let gates = parse_formal_gates(&flag_value(arguments, "--gates")?)?;
            reject_extra_arguments(arguments, &["--gates"])?;
            Ok(Command::FormalAudit(gates))
        }
        "regenerate-fixtures" => {
            reject_extra_arguments(arguments, &[])?;
            Ok(Command::RegenerateFixtures)
        }
        _ if subcommand.starts_with("--") => Err(usage(format!("unknown flag `{subcommand}`"))),
        _ => Err(usage(format!("unknown formal subcommand `{subcommand}`"))),
    }
}

fn required_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    description: &str,
) -> Result<String> {
    let argument = arguments
        .next()
        .ok_or_else(|| usage(format!("missing {description}")))?;
    argument
        .into_string()
        .map_err(|_| usage(format!("{description} must be valid UTF-8")))
}

fn expect_literal(
    arguments: &mut impl Iterator<Item = OsString>,
    expected: &str,
    description: &str,
) -> Result<()> {
    let value = required_argument(arguments, description)?;
    if value == expected {
        return Ok(());
    }
    if value.starts_with("--") {
        return Err(usage(format!("unknown flag `{value}`")));
    }
    Err(usage(format!(
        "expected {description} `{expected}`, found `{value}`"
    )))
}

fn expect_flag(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<()> {
    let value = required_argument(arguments, &format!("flag `{flag}`"))?;
    if value == flag {
        return Ok(());
    }
    if value.starts_with("--") {
        return Err(usage(format!("unknown flag `{value}`")));
    }
    Err(usage(format!("expected flag `{flag}`, found `{value}`")))
}

fn flag_value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String> {
    let value = required_argument(arguments, &format!("value for `{flag}`"))?;
    if !value.starts_with("--") {
        return Ok(value);
    }
    if value == flag {
        return Err(usage(format!("duplicate flag `{flag}`")));
    }
    Err(usage(format!("unknown flag `{value}`")))
}

fn reject_extra_arguments(
    arguments: &mut impl Iterator<Item = OsString>,
    known_flags: &[&str],
) -> Result<()> {
    let Some(value) = arguments.next() else {
        return Ok(());
    };
    let value = value
        .into_string()
        .map_err(|_| usage("extra argument must be valid UTF-8"))?;
    if known_flags.contains(&value.as_str()) {
        return Err(usage(format!("duplicate flag `{value}`")));
    }
    if value.starts_with("--") {
        return Err(usage(format!("unknown flag `{value}`")));
    }
    Err(usage(format!("unexpected argument `{value}`")))
}

fn parse_formal_gates(value: &str) -> Result<Vec<Gate>> {
    if value.is_empty() {
        return Err(usage("`--gates` must not be empty"));
    }

    let mut gates = Vec::new();
    for name in value.split(',') {
        if name.is_empty() {
            return Err(usage("`--gates` contains an empty gate"));
        }
        let gate = Gate::parse(name)?;
        if gates.contains(&gate) {
            return Err(usage(format!("duplicate gate `{name}`")));
        }
        gates.push(gate);
    }

    let dependency_order = Gate::FORMAL_ORDER;
    if gates.len() > dependency_order.len()
        || gates
            .iter()
            .zip(dependency_order)
            .any(|(actual, expected)| *actual != expected)
    {
        let expected = dependency_order.map(gate_name).join(",");
        return Err(VerificationError::new(
            ErrorCode::GateDependency,
            format!("formal audit gates must be a dependency-respecting prefix of {expected}"),
        ));
    }

    Ok(gates)
}

fn verify_g0(root: &Path) -> Result<()> {
    let workspace = audit_workspace(root)?;
    let ledger = verify_ledger_g0(root)?;
    let workspace_checks = report_checks(workspace, Gate::G0, "workspace G0 audit")?;
    let ledger_checks = report_checks(ledger, Gate::G0, "ledger G0 verification")?;
    let checks = ledger_checks
        .checked_add(workspace_checks)
        .ok_or_else(|| VerificationError::new(ErrorCode::Schema, "G0 check count overflow"))?;
    emit(&format!("G0 PASS checks={checks}\n"))
}

fn audit_formal(root: &Path, gates: &[Gate]) -> Result<()> {
    let formal_gates: Vec<_> = gates
        .iter()
        .copied()
        .filter(|gate| *gate != Gate::G6)
        .collect();
    let reports = audit_formal_gates(root, &formal_gates)?;
    let checks = ordered_report_checks(reports, &formal_gates)?;

    let mut output = String::new();
    for (gate, checks) in formal_gates.iter().zip(checks) {
        append_pass_line(&mut output, *gate, checks);
    }

    if gates.last() == Some(&Gate::G6) {
        let reports = audit_formal_artifacts(root, &[Gate::G6])?;
        let checks = ordered_report_checks(reports, &[Gate::G6])?
            .into_iter()
            .next()
            .ok_or_else(|| {
                VerificationError::new(ErrorCode::Schema, "G6 audit omitted its report")
            })?;
        qualify_replay_canary(root)?;
        append_g6_ready(&mut output, checks)?;
    }

    emit(&output)
}

fn regenerate_fixtures(root: &Path) -> Result<()> {
    let transitions = regenerate_canonical_fixture(root)?;
    emit(&format!("G6 FIXTURES transitions={transitions}\n"))
}

fn ordered_report_checks(reports: Vec<GateReport>, expected: &[Gate]) -> Result<Vec<usize>> {
    if reports.len() != expected.len() {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "gate audit returned {} reports for {} requested gates",
                reports.len(),
                expected.len()
            ),
        ));
    }

    let mut checks_by_gate = BTreeMap::new();
    for report in reports {
        if !expected.contains(&report.gate) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "gate audit returned unexpected report for {}",
                    gate_name(report.gate)
                ),
            ));
        }
        if checks_by_gate.insert(report.gate, report.checks).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "gate audit returned duplicate report for {}",
                    gate_name(report.gate)
                ),
            ));
        }
    }

    expected
        .iter()
        .map(|gate| {
            checks_by_gate.get(gate).copied().ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::Schema,
                    format!("gate audit omitted report for {}", gate_name(*gate)),
                )
            })
        })
        .collect()
}

fn report_checks(report: GateReport, expected: Gate, source: &str) -> Result<usize> {
    if report.gate == expected {
        return Ok(report.checks);
    }
    Err(VerificationError::new(
        ErrorCode::Schema,
        format!(
            "{source} returned {} instead of {}",
            gate_name(report.gate),
            gate_name(expected)
        ),
    ))
}

fn append_pass_line(output: &mut String, gate: Gate, checks: usize) {
    output.push_str(gate_name(gate));
    output.push_str(" PASS checks=");
    output.push_str(&checks.to_string());
    output.push('\n');
}

fn append_g6_ready(output: &mut String, checks: usize) -> Result<()> {
    let mut lanes = ready_lane_records();
    if lanes.len() != 6 {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!(
                "G6 READY registry contains {} lanes instead of 6",
                lanes.len()
            ),
        ));
    }
    if lanes.iter().any(|lane| lane.status != LaneStatus::Ready) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "G6 READY registry contains a non-READY lane",
        ));
    }

    lanes.sort_by(|left, right| left.lane.cmp(right.lane));
    if lanes.windows(2).any(|pair| pair[0].lane == pair[1].lane) {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "G6 READY registry contains duplicate lane names",
        ));
    }

    output.push_str("G6 READY lanes=6 checks=");
    output.push_str(&checks.to_string());
    output.push('\n');
    for lane in lanes {
        output.push_str("G6 READY lane=");
        output.push_str(lane.lane);
        output.push_str(" model=");
        output.push_str(lane.formal_model);
        output.push('\n');
    }
    Ok(())
}

fn emit(output: &str) -> Result<()> {
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|error| {
            VerificationError::new(ErrorCode::Io, format!("cannot write output: {error}"))
        })
}

fn gate_name(gate: Gate) -> &'static str {
    match gate {
        Gate::G0 => "G0",
        Gate::G1 => "G1",
        Gate::G2 => "G2",
        Gate::G3 => "G3",
        Gate::G4 => "G4",
        Gate::G5 => "G5",
        Gate::G6 => "G6",
    }
}

fn usage(detail: impl Into<String>) -> VerificationError {
    VerificationError::new(ErrorCode::Usage, detail)
}

#[cfg(test)]
mod tests {
    use bamts_verification::Gate;

    use super::parse_formal_gates;

    #[test]
    fn formal_gate_parser_uses_authoritative_dependency_order() {
        assert_eq!(
            parse_formal_gates("G1,G2,G5,G3").unwrap(),
            Gate::FORMAL_ORDER[..4]
        );
        assert!(parse_formal_gates("G1,G2,G3").is_err());
    }
}
