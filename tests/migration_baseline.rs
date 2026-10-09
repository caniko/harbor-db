#![cfg(feature = "testing")]
use harbor_db::testing::{baseline::Baseline, catalog::Suite};
use std::path::Path;

#[test]
fn operator_can_check_pr14_scope_without_executing_or_claiming_qualification() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .arg("check-baseline")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "baseline_retained");
    assert_eq!(receipt["runtime_qualified"], false);
    assert_eq!(receipt["gates"], 22);
}

#[test]
fn pr14_contract_preserves_full_native_inventory_and_all_qualified_gates() {
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let suite = Suite::load("tests/suite.toml").unwrap();
    baseline.validate_migration(Path::new("."), &suite).unwrap();
    assert_eq!(baseline.head, "4b2850507a2e9bdfe198caf9178ea7b99ffb03a5");
    assert_eq!(baseline.python_test_count, 174);
    assert_eq!(baseline.gates.len(), 22);
    assert!(
        baseline
            .files
            .iter()
            .any(|file| file.path == Path::new("docs/application-storage.md"))
    );
    assert!(
        baseline
            .modules
            .iter()
            .any(|module| module.source == Path::new("python/harbor_db/application_transition.py"))
    );
}

#[test]
fn missing_pr_case_or_gate_blocks_migration_admission() {
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let suite = Suite::load("tests/suite.toml").unwrap();
    let mut incomplete = suite.clone();
    let removed = baseline
        .python_tests(Path::new("."))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    incomplete
        .cases
        .retain(|case| case.python_migration_id.as_deref() != Some(&removed));
    assert!(
        baseline
            .validate_migration(Path::new("."), &incomplete)
            .unwrap_err()
            .to_string()
            .contains("Python")
    );
    let mut incomplete = suite;
    incomplete.cases.retain(|case| !matches!(&case.execution,
        harbor_db::testing::catalog::Execution::Nix { installable } if installable == &baseline.gates[0]));
    assert!(
        baseline
            .validate_migration(Path::new("."), &incomplete)
            .unwrap_err()
            .to_string()
            .contains("gate")
    );
}

#[test]
fn baseline_rejects_duplicate_mappings_traversal_and_unreviewed_head() {
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let mut changed = baseline.clone();
    changed.modules.push(changed.modules[0].clone());
    assert!(changed.validate().is_err());
    changed = baseline.clone();
    changed.files[0].path = "../escaped".into();
    assert!(changed.validate().is_err());
    changed = baseline;
    changed.head = "ef0315e5b2ff9e1770b47b1c270b74851ba484b6".into();
    assert!(changed.validate().is_err());
}

#[test]
fn baseline_cannot_keep_counts_while_dropping_application_contracts_or_changing_gates() {
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let mut changed = baseline.clone();
    changed
        .files
        .retain(|file| file.path != Path::new("nix/test-application-transition.nix"));
    assert!(changed.validate().is_err());
    changed = baseline;
    changed.gates[0] = ".#checks.x86_64-linux.unqualified-substitute".into();
    assert!(changed.validate().is_err());
}
