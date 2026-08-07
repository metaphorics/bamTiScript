#![forbid(unsafe_code)]

use std::io::{self, Write};

use bamts_cli::args::{explain_rule, help_message, parse_env_args, version_message};
use bamts_cli::driver::{DriverError, execute};

fn main() {
    let exit_code = match parse_env_args() {
        Ok(args) if args.help => write_stdout(help_message().as_bytes()),
        Ok(args) if args.version => write_stdout(format!("{}\n", version_message()).as_bytes()),
        Ok(args) if args.is_explain() => match args.explain_rule.as_deref().map(explain_rule) {
            Some(Ok(explanation)) => write_stdout(explanation.as_bytes()),
            Some(Err(error)) => report_usage_error(&error),
            None => 2,
        },
        Ok(args) => match execute(&args) {
            Ok(outcome) => {
                if write_stderr(&outcome.stderr) != 0 || write_stdout(&outcome.stdout) != 0 {
                    1
                } else {
                    outcome.exit_code
                }
            }
            Err(error) => report_driver_error(&error),
        },
        Err(error) => {
            let message = format!("error: {error}\n\n{}", help_message());
            if write_stderr(message.as_bytes()) == 0 {
                2
            } else {
                1
            }
        }
    };
    std::process::exit(exit_code);
}

fn report_driver_error(error: &DriverError) -> i32 {
    if error.is_usage_error() {
        return report_usage_error(error);
    }
    let message = error.rendered_diagnostic().map_or_else(
        || format!("error: {error}\n"),
        |rendered| rendered.to_owned(),
    );
    let _ = write_stderr(message.as_bytes());
    1
}

fn report_usage_error(error: &impl std::fmt::Display) -> i32 {
    let message = format!("error: {error}\n\n{}", help_message());
    if write_stderr(message.as_bytes()) == 0 {
        2
    } else {
        1
    }
}

fn write_stdout(bytes: &[u8]) -> i32 {
    write_all(io::stdout().lock(), bytes)
}

fn write_stderr(bytes: &[u8]) -> i32 {
    write_all(io::stderr().lock(), bytes)
}

fn write_all(mut writer: impl Write, bytes: &[u8]) -> i32 {
    match writer.write_all(bytes) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
