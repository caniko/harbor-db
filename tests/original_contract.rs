#![cfg(feature = "testing")]

use harbor_db::testing::{baseline::Baseline, catalog::Suite};
use std::{collections::BTreeSet, path::Path, process::Command};

#[test]
fn native_export_retains_every_original_selector_and_disclaims_execution() {
    let output = Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .arg("original-contract")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contract: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let required = baseline.python_tests(Path::new(".")).unwrap();
    let methods = contract["methods"].as_object().unwrap();
    assert_eq!(methods.keys().cloned().collect::<BTreeSet<_>>(), required);
    assert_eq!(methods.len(), 174);
    assert_eq!(
        contract["schema"],
        "harbor-db.original-obligation-contract.v1"
    );
    assert_eq!(contract["scope"], "original-obligation-inventory");
    assert_eq!(contract["executed"], false);
    assert_eq!(contract["runtime_qualified"], false);
    assert_eq!(
        contract["method_inventory_sha256"],
        baseline.python_inventory_sha256
    );
    assert_eq!(contract["baseline_head"], baseline.head);
    assert_eq!(contract["original_gates"].as_array().unwrap().len(), 22);
    for method in methods.values() {
        assert!(!method["catalog_cases"].as_array().unwrap().is_empty());
        assert_eq!(method["oracle_sha256"].as_str().unwrap().len(), 64);
    }
    let repeated = Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .arg("original-contract")
        .output()
        .unwrap();
    assert!(repeated.status.success());
    assert_eq!(output.stdout, repeated.stdout);
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
            .arg("original-contract")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let workers = [spawn(), spawn()];
    for worker in workers {
        let result = worker.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(output.stdout, result.stdout);
    }
}

#[test]
fn original_contract_cannot_select_a_successful_subset_or_drop_original_gates() {
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let suite = Suite::load("tests/suite.toml").unwrap();
    let selector = baseline
        .python_tests(Path::new("."))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut incomplete = suite.clone();
    incomplete
        .cases
        .retain(|case| case.python_migration_id.as_deref() != Some(&selector));
    assert!(
        baseline
            .original_contract(Path::new("."), &incomplete)
            .is_err()
    );
    let mut incomplete = suite;
    incomplete.cases.retain(|case| !matches!(&case.execution,
        harbor_db::testing::catalog::Execution::Nix { installable } if installable == &baseline.gates[0]));
    assert!(
        baseline
            .original_contract(Path::new("."), &incomplete)
            .is_err()
    );
}
