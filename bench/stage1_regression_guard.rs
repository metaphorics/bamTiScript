// Thin bench wrapper for the E5.4 stage-1 pre-registered regression guard.
//
// The evaluation logic lives in `crates/bamts-verification/src/perf_guard.rs`.
// This file only reads the three registered artifacts and exits deterministically:
//   0 = pass
//   1 = guard failure (regression, condition mismatch, missing metric, etc.)
//   2 = input/measurement error

use std::env;
use std::path::PathBuf;
use std::process;

use bamts_verification::perf_guard::{Verdict, evaluate_stage1_guard_from_paths};

fn main() {
    let mut args = env::args().skip(1);
    let rules = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bench/compiler-rules.toml"));
    let baseline = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bench/baselines/stage1.json"));
    let scorecard = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bench/scorecards/stage1.json"));

    match evaluate_stage1_guard_from_paths(&rules, &baseline, &scorecard) {
        Ok(Verdict::Pass {
            current_median,
            bound,
        }) => {
            println!("PASS median={current_median} bound={bound}");
            process::exit(0);
        }
        Ok(Verdict::Fail(reason)) => {
            eprintln!("FAIL: {reason}");
            process::exit(1);
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            process::exit(2);
        }
    }
}
