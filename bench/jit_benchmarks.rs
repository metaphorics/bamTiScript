//! E5.2 JIT benchmark leaf wrapper.
//!
//! The implementation lives in `crates/bamts-verification/src/perf_jit.rs`;
//! this file is the thin entry point the completion ownership map expects at
//! `bench/jit_benchmarks.rs`.

use bamts_verification::perf_jit;

fn main() {
    match perf_jit::run() {
        Ok(receipt) => println!("{}", receipt),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
