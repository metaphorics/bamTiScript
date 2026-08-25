use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process,
};

use bamts_verification::{
    fixtures::{materialize_fixtures, verify_fixtures},
    perf::{
        MeasureOptions, PerfError, Result, ScorecardOptions, bless_baseline, capture_scorecard,
        check_baseline, compare, load_host, load_manifest, load_policy, load_result,
        load_scorecard, measure, read_machine_fingerprint, validate_scorecard_on_machine,
    },
};

enum Command {
    Measure(MeasureOptions),
    Compare {
        host_path: PathBuf,
        policy_path: PathBuf,
        result_path: PathBuf,
    },
    MaterializeFixtures {
        manifest_path: PathBuf,
        workspace_root: PathBuf,
    },
    VerifyFixtures {
        manifest_path: PathBuf,
        workspace_root: PathBuf,
    },
    BlessBaseline {
        host_path: PathBuf,
        result_path: PathBuf,
        out_path: PathBuf,
    },
    CheckBaseline {
        host_path: PathBuf,
        baseline_path: PathBuf,
    },
    CaptureScorecard(ScorecardOptions),
    CheckScorecard {
        host_path: PathBuf,
        scorecard_path: PathBuf,
        policy_path: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(io::stderr().lock(), "{error}");
        process::exit(error.code.exit_code());
    }
}

fn run() -> Result<()> {
    match parse_command(env::args_os())? {
        Command::Measure(options) => {
            let result = measure(&options)?;
            println!(
                "measured host={} slice={} benchmark={} repeats={} conditions_match={} out={}",
                result.host,
                result.slice,
                result.benchmark_id,
                result.repeats,
                result.conditions_match,
                options.out_path.display()
            );
        }
        Command::Compare {
            host_path,
            policy_path,
            result_path,
        } => {
            let host = load_host(&host_path)?;
            let policy = load_policy(&policy_path)?;
            let result = load_result(&result_path)?;
            let machine = read_machine_fingerprint()?;
            compare(&result, &policy, &host, &machine)?;
            println!(
                "budget_ok host={} slice={} benchmark={}",
                result.host, result.slice, result.benchmark_id
            );
        }
        Command::MaterializeFixtures {
            manifest_path,
            workspace_root,
        } => {
            let manifest = load_manifest(&manifest_path)?;
            for fixture in materialize_fixtures(&workspace_root, &manifest)? {
                println!(
                    "materialized {} {} {}",
                    fixture.id, fixture.sha256, fixture.bytes
                );
            }
        }
        Command::VerifyFixtures {
            manifest_path,
            workspace_root,
        } => {
            let manifest = load_manifest(&manifest_path)?;
            for fixture in verify_fixtures(&workspace_root, &manifest)?.fixtures {
                println!("ok {} {}", fixture.id, fixture.sha256);
            }
        }
        Command::BlessBaseline {
            host_path,
            result_path,
            out_path,
        } => {
            bless_baseline(&host_path, &result_path, &out_path)?;
            println!("baseline_blessed out={}", out_path.display());
        }
        Command::CheckBaseline {
            host_path,
            baseline_path,
        } => {
            check_baseline(&host_path, &baseline_path)?;
            println!("baseline_ok path={}", baseline_path.display());
        }
        Command::CaptureScorecard(options) => {
            let card = capture_scorecard(&options)?;
            println!(
                "scorecard_captured comparator={} fixtures={} out={}",
                card.comparator,
                card.fixtures.len(),
                options.out_path.display()
            );
        }
        Command::CheckScorecard {
            host_path,
            scorecard_path,
            policy_path,
        } => {
            let host = load_host(&host_path)?;
            let card = load_scorecard(&scorecard_path)?;
            let comparator = if let Some(path) = policy_path {
                load_policy(&path)?.release.comparator
            } else {
                "typescript@7.0.2".to_owned()
            };
            let machine = read_machine_fingerprint()?;
            validate_scorecard_on_machine(&card, &host, &comparator, &machine)?;
            println!("scorecard_ok path={}", scorecard_path.display());
        }
    }
    Ok(())
}

fn parse_command(mut arguments: impl Iterator<Item = OsString>) -> Result<Command> {
    let _ = arguments.next();
    let command = required_argument(&mut arguments, "command")?;
    let cwd = env::current_dir().map_err(|error| {
        PerfError::harness(format!("cannot determine current directory: {error}"))
    })?;

    match command.as_str() {
        "measure" => parse_measure(&mut arguments, cwd),
        "compare" => parse_compare(&mut arguments),
        "materialize-fixtures" => Ok(Command::MaterializeFixtures {
            manifest_path: parse_one_path(&mut arguments, "--manifest", "materialize-fixtures")?,
            workspace_root: cwd,
        }),
        "verify-fixtures" => Ok(Command::VerifyFixtures {
            manifest_path: parse_one_path(&mut arguments, "--manifest", "verify-fixtures")?,
            workspace_root: cwd,
        }),
        "bless-baseline" => parse_three_paths(
            &mut arguments,
            "bless-baseline",
            ["--host", "--result", "--out"],
        )
        .map(
            |[host_path, result_path, out_path]| Command::BlessBaseline {
                host_path,
                result_path,
                out_path,
            },
        ),
        "check-baseline" => {
            parse_two_paths(&mut arguments, "check-baseline", ["--host", "--baseline"]).map(
                |[host_path, baseline_path]| Command::CheckBaseline {
                    host_path,
                    baseline_path,
                },
            )
        }
        "capture-scorecard" => parse_three_paths(
            &mut arguments,
            "capture-scorecard",
            ["--host", "--manifest", "--out"],
        )
        .map(|[host_path, manifest_path, out_path]| {
            Command::CaptureScorecard(ScorecardOptions {
                host_path,
                manifest_path,
                out_path,
                workspace_root: cwd,
            })
        }),
        "check-scorecard" => parse_check_scorecard(&mut arguments),
        _ if command.starts_with("--") => {
            Err(PerfError::usage(format!("unknown flag `{command}`")))
        }
        _ => Err(PerfError::usage(format!("unknown command `{command}`"))),
    }
}

fn parse_measure(arguments: &mut impl Iterator<Item = OsString>, cwd: PathBuf) -> Result<Command> {
    let mut host_path = None;
    let mut manifest_path = None;
    let mut slice = None;
    let mut out_path = None;
    let mut baseline_path = None;

    while let Some((flag, value)) = next_pair(arguments)? {
        match flag.as_str() {
            "--host" => host_path = Some(PathBuf::from(value)),
            "--manifest" => manifest_path = Some(PathBuf::from(value)),
            "--slice" => slice = Some(value),
            "--out" => out_path = Some(PathBuf::from(value)),
            "--baseline" => baseline_path = Some(PathBuf::from(value)),
            _ => return Err(PerfError::usage(format!("unknown flag `{flag}`"))),
        }
    }

    Ok(Command::Measure(MeasureOptions {
        host_path: host_path.ok_or_else(|| PerfError::usage("measure requires --host <path>"))?,
        manifest_path: manifest_path
            .ok_or_else(|| PerfError::usage("measure requires --manifest <path>"))?,
        slice: slice.ok_or_else(|| PerfError::usage("measure requires --slice <id>"))?,
        out_path: out_path.ok_or_else(|| PerfError::usage("measure requires --out <path>"))?,
        baseline_path,
        workspace_root: cwd.clone(),
        snapshot_root: cwd.join("verification/ts-suite"),
    }))
}

fn parse_compare(arguments: &mut impl Iterator<Item = OsString>) -> Result<Command> {
    let [host_path, policy_path, result_path] =
        parse_three_paths(arguments, "compare", ["--host", "--policy", "--result"])?;
    Ok(Command::Compare {
        host_path,
        policy_path,
        result_path,
    })
}

fn parse_check_scorecard(arguments: &mut impl Iterator<Item = OsString>) -> Result<Command> {
    let mut host_path = None;
    let mut scorecard_path = None;
    let mut policy_path = None;
    while let Some((flag, value)) = next_pair(arguments)? {
        match flag.as_str() {
            "--host" => host_path = Some(PathBuf::from(value)),
            "--scorecard" => scorecard_path = Some(PathBuf::from(value)),
            "--policy" => policy_path = Some(PathBuf::from(value)),
            _ => return Err(PerfError::usage(format!("unknown flag `{flag}`"))),
        }
    }
    Ok(Command::CheckScorecard {
        host_path: host_path
            .ok_or_else(|| PerfError::usage("check-scorecard requires --host <path>"))?,
        scorecard_path: scorecard_path
            .ok_or_else(|| PerfError::usage("check-scorecard requires --scorecard <path>"))?,
        policy_path,
    })
}

fn parse_one_path(
    arguments: &mut impl Iterator<Item = OsString>,
    flag: &str,
    command: &str,
) -> Result<PathBuf> {
    let mut value = None;
    while let Some((observed, raw)) = next_pair(arguments)? {
        if observed != flag {
            return Err(PerfError::usage(format!("unknown flag `{observed}`")));
        }
        if value.replace(PathBuf::from(raw)).is_some() {
            return Err(PerfError::usage(format!("duplicate flag `{flag}`")));
        }
    }
    value.ok_or_else(|| PerfError::usage(format!("{command} requires {flag} <path>")))
}

fn parse_two_paths(
    arguments: &mut impl Iterator<Item = OsString>,
    command: &str,
    flags: [&str; 2],
) -> Result<[PathBuf; 2]> {
    let values = parse_paths(arguments, command, &flags)?;
    Ok([values[0].clone(), values[1].clone()])
}

fn parse_three_paths(
    arguments: &mut impl Iterator<Item = OsString>,
    command: &str,
    flags: [&str; 3],
) -> Result<[PathBuf; 3]> {
    let values = parse_paths(arguments, command, &flags)?;
    Ok([values[0].clone(), values[1].clone(), values[2].clone()])
}

fn parse_paths(
    arguments: &mut impl Iterator<Item = OsString>,
    command: &str,
    flags: &[&str],
) -> Result<Vec<PathBuf>> {
    let mut values = vec![None; flags.len()];
    while let Some((flag, raw)) = next_pair(arguments)? {
        let index = flags
            .iter()
            .position(|candidate| *candidate == flag)
            .ok_or_else(|| PerfError::usage(format!("unknown flag `{flag}`")))?;
        if values[index].replace(PathBuf::from(raw)).is_some() {
            return Err(PerfError::usage(format!("duplicate flag `{flag}`")));
        }
    }
    flags
        .iter()
        .zip(values)
        .map(|(flag, value)| {
            value.ok_or_else(|| PerfError::usage(format!("{command} requires {flag} <path>")))
        })
        .collect()
}

fn next_pair(arguments: &mut impl Iterator<Item = OsString>) -> Result<Option<(String, String)>> {
    let Some(raw_flag) = arguments.next() else {
        return Ok(None);
    };
    let flag = raw_flag
        .into_string()
        .map_err(|_| PerfError::usage("flag must be valid UTF-8"))?;
    if !flag.starts_with("--") {
        return Err(PerfError::usage(format!("unexpected argument `{flag}`")));
    }
    let value = flag_value(arguments, &flag)?;
    Ok(Some((flag, value)))
}

fn flag_value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String> {
    let value = required_argument(arguments, &format!("value for `{flag}`"))?;
    if value.starts_with("--") {
        return Err(PerfError::usage(format!("missing value for `{flag}`")));
    }
    Ok(value)
}

fn required_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    description: &str,
) -> Result<String> {
    let argument = arguments
        .next()
        .ok_or_else(|| PerfError::usage(format!("missing {description}")))?;
    argument
        .into_string()
        .map_err(|_| PerfError::usage(format!("{description} must be valid UTF-8")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_requires_host_manifest() {
        let error = match parse_command(
            [
                "perf_budget",
                "compare",
                "--policy",
                "policy.toml",
                "--result",
                "result.json",
            ]
            .into_iter()
            .map(OsString::from),
        ) {
            Ok(_) => panic!("compare without --host must fail"),
            Err(error) => error,
        };
        assert_eq!(error.code.as_str(), "USAGE");
        assert!(error.detail.contains("--host"));
    }

    #[test]
    fn compare_accepts_host_policy_and_result_in_any_order() {
        let command = parse_command(
            [
                "perf_budget",
                "compare",
                "--result",
                "result.json",
                "--host",
                "bh1.toml",
                "--policy",
                "budgets.toml",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();
        let Command::Compare {
            host_path,
            policy_path,
            result_path,
        } = command
        else {
            panic!("expected compare command");
        };
        assert_eq!(host_path, PathBuf::from("bh1.toml"));
        assert_eq!(policy_path, PathBuf::from("budgets.toml"));
        assert_eq!(result_path, PathBuf::from("result.json"));
    }
}
