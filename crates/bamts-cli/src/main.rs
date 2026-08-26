#![forbid(unsafe_code)]

use std::io::{self, Write};

use bamts_cli::api_server;
use bamts_cli::cli::tsc_args::{api_transport_requested, parse_tsc_args};
use bamts_cli::driver::{DriverError, execute_tsc};

fn main() {
    let argv = std::env::args().skip(1).collect::<Vec<_>>();
    if api_transport_requested(&argv) {
        let exit_code = api_server::maybe_run(&argv).expect("--api selects the API transport");
        std::process::exit(exit_code);
    }
    let exit_code = match parse_tsc_args(&argv) {
        Ok(command) => match execute_tsc(&command) {
            Ok(outcome) => {
                if write_stderr(&outcome.stderr) != 0 || write_stdout(&outcome.stdout) != 0 {
                    1
                } else {
                    outcome.exit_code
                }
            }
            Err(error) => report_driver_error(&error),
        },
        Err(errors) => {
            if write_stderr(errors.pretty_false().as_bytes()) == 0 {
                errors.exit_status().code()
            } else {
                1
            }
        }
    };
    std::process::exit(exit_code);
}

fn report_driver_error(error: &DriverError) -> i32 {
    let message = error.rendered_diagnostic().map_or_else(
        || format!("error: {error}\n"),
        |rendered| rendered.to_owned(),
    );
    let _ = write_stderr(message.as_bytes());
    1
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
