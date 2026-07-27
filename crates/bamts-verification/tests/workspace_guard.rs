use bamts_verification::{Gate, workspace_guard::audit_workspace};
use std::path::Path;

#[test]
fn approved_workspace_satisfies_guard() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report = audit_workspace(&root).expect("approved workspace must satisfy the guard");

    assert_eq!(report.gate, Gate::G0);
    assert!(report.checks > 0);
}
