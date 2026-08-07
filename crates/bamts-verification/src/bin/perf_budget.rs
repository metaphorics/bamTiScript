use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process,
};

use bamts_verification::perf::{
    MeasureOptions, PerfError, Result, compare, load_policy, load_result, measure,
    read_machine_fingerprint,
};

enum Command {
    Measure(MeasureOptions),
    Compare {
        policy_path: PathBuf,
        result_path: PathBuf,
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
            Ok(())
        }
        Command::Compare {
            policy_path,
            result_path,
        } => {
            let policy = load_policy(&policy_path)?;
            let result = load_result(&result_path)?;
            let machine = read_machine_fingerprint()?;
            compare(&result, &policy, &machine)?;
            println!(
                "budget_ok host={} slice={} benchmark={}",
                result.host, result.slice, result.benchmark_id
            );
            Ok(())
        }
    }
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

    while let Some(raw) = arguments.next() {
        let flag = raw
            .into_string()
            .map_err(|_| PerfError::usage("flag must be valid UTF-8"))?;
        match flag.as_str() {
            "--host" => host_path = Some(path_value(arguments, "--host")?),
            "--manifest" => manifest_path = Some(path_value(arguments, "--manifest")?),
            "--slice" => slice = Some(flag_value(arguments, "--slice")?),
            "--out" => out_path = Some(path_value(arguments, "--out")?),
            "--baseline" => baseline_path = Some(path_value(arguments, "--baseline")?),
            _ if flag.starts_with("--") => {
                return Err(PerfError::usage(format!("unknown flag `{flag}`")));
            }
            _ => return Err(PerfError::usage(format!("unexpected argument `{flag}`"))),
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
    let mut policy_path = None;
    let mut result_path = None;

    while let Some(raw) = arguments.next() {
        let flag = raw
            .into_string()
            .map_err(|_| PerfError::usage("flag must be valid UTF-8"))?;
        match flag.as_str() {
            "--policy" => policy_path = Some(path_value(arguments, "--policy")?),
            "--result" => result_path = Some(path_value(arguments, "--result")?),
            _ if flag.starts_with("--") => {
                return Err(PerfError::usage(format!("unknown flag `{flag}`")));
            }
            _ => return Err(PerfError::usage(format!("unexpected argument `{flag}`"))),
        }
    }

    Ok(Command::Compare {
        policy_path: policy_path
            .ok_or_else(|| PerfError::usage("compare requires --policy <path>"))?,
        result_path: result_path
            .ok_or_else(|| PerfError::usage("compare requires --result <path>"))?,
    })
}

fn path_value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(flag_value(arguments, flag)?))
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
