//! Thin wrapper for E5.3 stage-0 evidence.
//!
//! Delegates to `bamts_verification::perf_stage0` for condition capture,
//! baseline comparison, and digest-bound receipt emission.  This binary only
//! prints the receipt; all measurement logic lives in the verification crate.

#[cfg(not(test))]
use bamts_verification::perf_stage0;

#[cfg(not(test))]
fn main() {
    let receipt = perf_stage0::run();
    println!(
        "{}",
        serde_json::to_string(&receipt).expect("serialize receipt")
    );
}

#[cfg(test)]
mod tests {
    use bamts_verification::perf_stage0;

    #[test]
    fn wrapper_emits_receipt() {
        let receipt = perf_stage0::run();
        assert_eq!(
            receipt.get("schema").and_then(|s| s.as_str()),
            Some("bamti.evidence/v1")
        );
        assert!(receipt.get("state").is_some());
        let state = receipt.get("state").and_then(|s| s.as_str()).unwrap_or("");
        assert_ne!(state, "PASS");
        assert!(receipt.get("compiler_rules_digest").is_some());
        assert!(receipt.get("catalog_inputs_digest").is_some());
    }
}
