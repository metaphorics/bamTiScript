#![forbid(unsafe_code)]

use std::io::{self, Write};

fn main() {
    let outcome = bamts_cli::cli::cli_outcome(std::env::args());
    let exit_code = if write_stderr(&outcome.stderr) != 0 || write_stdout(&outcome.stdout) != 0 {
        1
    } else {
        outcome.exit_code
    };
    std::process::exit(exit_code);
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
