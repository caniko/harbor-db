#![cfg(feature = "testing")]
use harbor_db::testing::{
    baseline::{Baseline, FileRole},
    catalog::Suite,
};
use std::{fs, path::Path};

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

fn extension_fixture() -> (tempfile::TempDir, Baseline, Suite) {
    let root = tempfile::tempdir().unwrap();
    let baseline = Baseline::load("tests/pr14-baseline.toml").unwrap();
    let suite = Suite::load("tests/suite.toml").unwrap();
    let copy = |path: &Path| {
        let destination = root.path().join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(path, destination).unwrap();
    };
    for file in &baseline.files {
        if matches!(file.role, FileRole::Runtime | FileRole::Test) {
            copy(&file.path);
        }
        if file.role == FileRole::Runtime {
            copy(&Path::new("tests/oracles/pr14").join(&file.path));
        }
    }
    copy(Path::new("tests/runtime-extensions.toml"));
    for path in baseline.modules.iter().flat_map(|mapping| {
        mapping.rust.iter().cloned().chain(
            mapping
                .commands
                .iter()
                .map(|command| format!("src/bin/{command}.rs").into()),
        )
    }) {
        let destination = root.path().join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, "").unwrap();
    }
    let mut ci: toml::Value = toml::from_str(&fs::read_to_string("simit.toml").unwrap()).unwrap();
    let gates = ci["ci"]["nix_builds"].as_array_mut().unwrap();
    for gate in [
        ".#checks.x86_64-linux.postgres-lifecycle-test",
        ".#checks.x86_64-linux.postgres-lifecycle-oracle-test",
        ".#checks.x86_64-linux.native-source-local-recovery",
    ] {
        if !gates.iter().any(|value| value.as_str() == Some(gate)) {
            gates.push(gate.into());
        }
    }
    fs::write(
        root.path().join("simit.toml"),
        toml::to_string(&ci).unwrap(),
    )
    .unwrap();
    baseline.validate_migration(root.path(), &suite).unwrap();
    (root, baseline, suite)
}

fn edit_extension_manifest(root: &Path, edit: impl FnOnce(&mut toml::Value)) {
    let path = root.join("tests/runtime-extensions.toml");
    let mut manifest = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut manifest);
    fs::write(path, toml::to_string(&manifest).unwrap()).unwrap();
}

#[test]
fn extension_requires_exact_hash_and_explicit_changed_scope() {
    for path in ["python/harbor_db/backup.py", "python/harbor_db/process.py"] {
        let (root, baseline, suite) = extension_fixture();
        fs::write(root.path().join(path), "unapproved edit\n").unwrap();
        let error = baseline
            .validate_migration(root.path(), &suite)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unapproved runtime extension hash"),
            "{error}"
        );
        assert!(error.contains(path), "{error}");
    }
    let (root, baseline, suite) = extension_fixture();
    fs::copy(
        root.path()
            .join("tests/oracles/pr14/python/harbor_db/backup.py"),
        root.path().join("python/harbor_db/backup.py"),
    )
    .unwrap();
    assert!(
        baseline
            .validate_migration(root.path(), &suite)
            .unwrap_err()
            .to_string()
            .contains("scope differs")
    );
}

#[test]
fn extension_requires_every_frozen_oracle_even_for_unchanged_runtime() {
    for path in ["python/harbor_db/backup.py", "python/harbor_db/process.py"] {
        for missing in [false, true] {
            let (root, baseline, suite) = extension_fixture();
            let oracle = root.path().join("tests/oracles/pr14").join(path);
            if missing {
                fs::remove_file(oracle).unwrap();
            } else {
                fs::write(oracle, "rewritten oracle\n").unwrap();
            }
            assert!(baseline.validate_migration(root.path(), &suite).is_err());
        }
    }
}

#[test]
fn extension_manifest_rejects_typos_provenance_paths_and_invalid_hashes() {
    for (field, value) in [
        ("version", toml::Value::Integer(2)),
        ("baseline_head", "unreviewed".into()),
        ("typo", true.into()),
    ] {
        let (root, baseline, suite) = extension_fixture();
        edit_extension_manifest(root.path(), |manifest| {
            manifest.as_table_mut().unwrap().insert(field.into(), value);
        });
        assert!(
            baseline.validate_migration(root.path(), &suite).is_err(),
            "{field}"
        );
    }
    for path in [
        "../backup.py",
        "/python/harbor_db/backup.py",
        "tests/pr14-baseline.toml",
        "python/tests/test_backup.py",
        "unknown.py",
    ] {
        let (root, baseline, suite) = extension_fixture();
        edit_extension_manifest(root.path(), |manifest| {
            manifest["extensions"][0]["path"] = path.into();
        });
        assert!(
            baseline.validate_migration(root.path(), &suite).is_err(),
            "{path}"
        );
    }
    for hash in ["ABCDEF", &"A".repeat(64), &"0".repeat(64)] {
        let (root, baseline, suite) = extension_fixture();
        edit_extension_manifest(root.path(), |manifest| {
            manifest["extensions"][0]["sha256"] = hash.into();
        });
        assert!(
            baseline.validate_migration(root.path(), &suite).is_err(),
            "{hash}"
        );
    }
    let (root, baseline, suite) = extension_fixture();
    edit_extension_manifest(root.path(), |manifest| {
        let extension = manifest["extensions"][0].clone();
        manifest["extensions"]
            .as_array_mut()
            .unwrap()
            .push(extension);
    });
    assert!(baseline.validate_migration(root.path(), &suite).is_err());
}

#[test]
fn extension_gates_cases_and_required_vm_artifacts_cannot_be_substituted() {
    for field in ["gates", "qualified_cases"] {
        for empty in [false, true] {
            let (root, baseline, suite) = extension_fixture();
            edit_extension_manifest(root.path(), |manifest| {
                let values = manifest[field].as_array_mut().unwrap();
                if empty {
                    values.clear();
                } else {
                    values.push("nonsense".into());
                }
            });
            assert!(
                baseline.validate_migration(root.path(), &suite).is_err(),
                "{field}"
            );
        }
    }
    let (root, baseline, mut suite) = extension_fixture();
    let case = suite
        .cases
        .iter_mut()
        .find(|case| case.id == "vm.x86_64-linux.native-source-local-recovery")
        .unwrap();
    case.artifact_specs.retain(|artifact| {
        !matches!(
            artifact.kind,
            harbor_db::testing::evidence::ArtifactKind::Junit
        )
    });
    assert!(
        baseline
            .validate_migration(root.path(), &suite)
            .unwrap_err()
            .to_string()
            .contains("artifacts")
    );
    let (root, baseline, suite) = extension_fixture();
    let path = root.path().join("simit.toml");
    let mut ci: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    ci["ci"]["nix_builds"]
        .as_array_mut()
        .unwrap()
        .retain(|value| {
            value.as_str() != Some(".#checks.x86_64-linux.postgres-lifecycle-oracle-test")
        });
    fs::write(path, toml::to_string(&ci).unwrap()).unwrap();
    assert!(
        baseline
            .validate_migration(root.path(), &suite)
            .unwrap_err()
            .to_string()
            .contains("removed from CI")
    );
}

#[test]
fn absent_extension_manifest_keeps_original_runtime_hash_requirement() {
    let (root, baseline, suite) = extension_fixture();
    fs::remove_file(root.path().join("tests/runtime-extensions.toml")).unwrap();
    assert!(
        baseline
            .validate_migration(root.path(), &suite)
            .unwrap_err()
            .to_string()
            .contains("runtime baseline changed")
    );
    for file in baseline
        .files
        .iter()
        .filter(|file| file.role == FileRole::Runtime)
    {
        fs::copy(
            root.path().join("tests/oracles/pr14").join(&file.path),
            root.path().join(&file.path),
        )
        .unwrap();
    }
    baseline.validate_migration(root.path(), &suite).unwrap();
}
