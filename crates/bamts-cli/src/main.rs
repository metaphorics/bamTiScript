#![forbid(unsafe_code)]

use std::io::{self, Write};

use bamts_cli::api_server;
use bamts_cli::cli::tsc_args::{api_transport_requested, lsp_transport_requested, parse_tsc_args};
use bamts_cli::driver::{DriverError, execute_tsc};
use bamts_cli::lsp;

fn main() {
    let argv = std::env::args().skip(1).collect::<Vec<_>>();
    if api_transport_requested(&argv) {
        let exit_code = api_server::maybe_run(&argv).expect("--api selects the API transport");
        std::process::exit(exit_code);
    }
    if lsp_transport_requested(&argv) {
        std::process::exit(run_lsp());
    }
    if bamts_cli::cli::native_execution_requested(&argv) {
        let outcome = bamts_cli::cli::cli_outcome(argv);
        if write_stderr(&outcome.stderr) != 0 || write_stdout(&outcome.stdout) != 0 {
            std::process::exit(1);
        }
        std::process::exit(outcome.exit_code);
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
            if write_stdout(errors.pretty_false().as_bytes()) == 0 {
                errors.exit_status().code()
            } else {
                1
            }
        }
    };
    std::process::exit(exit_code);
}

fn run_lsp() -> i32 {
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("bamts: {error}");
            return 1;
        }
    };
    match lsp::run(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        root,
    ) {
        Ok(lsp::Exit::Shutdown) => 0,
        Ok(lsp::Exit::Unrequested) | Err(_) => 1,
    }
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
