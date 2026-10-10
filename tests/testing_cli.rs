#![cfg(feature = "testing")]
use harbor_db::testing::{
    catalog::Profile,
    executor::ExecutorSpec,
    runner,
    supervisor::{self, Verdict},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .args(args)
        .output()
        .unwrap()
}
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn repo(root: &Path) -> PathBuf {
    let source = root.join("checkout");
    fs::create_dir(&source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::write(source.join("source.txt"), "candidate input").unwrap();
    source
}
fn suite(root: &Path, selectors: &[(&str, &str)], covered: bool) -> PathBuf {
    let path = root.join("suite.toml");
    let mut toml = String::from(
        "version = 1\n[[capabilities]]\nid = 'example'\noperations = ['execute']\nrequired = ['positive', 'rejection', 'repetition', 'concurrency', 'interruption', 'recovery', 'compatibility']\n",
    );
    if covered {
        for dimension in [
            "rejection",
            "repetition",
            "concurrency",
            "interruption",
            "recovery",
            "compatibility",
        ] {
            toml.push_str(&format!("[[capabilities.not_applicable]]\ndimension = '{dimension}'\nreason = 'fixture only exercises positive execution'\n"));
        }
    }
    for (id, command) in selectors {
        toml.push_str(&format!("[[cases]]\nid = '{id}'\nprofile = 'fast'\nmaturity = 'prototype'\nsource = 'fixture'\ndeadline_seconds = 10\nresources = {{cpu = 1, memory_mib = 64, disk_mib = 64, exclusive = []}}\nplatforms = ['{}-linux']\ndepends_on = []\nartifacts = []\ncoverage_note = 'fixture'\nexecution = {{kind = 'argv', argv = ['python3', '-B', '-m', 'unittest', '{command}']}}\npython_migration_id = '{command}'\n", std::env::consts::ARCH));
        if covered {
            toml.push_str("[[cases.coverage]]\ncapability = 'example'\noperation = 'execute'\ndimension = 'positive'\nevidence = 'individual unittest result'\n");
        }
    }
    fs::write(&path, toml).unwrap();
    path
}
fn explicit(root: &Path, command: &str) -> (PathBuf, PathBuf) {
    let source = root.join("snapshot");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("bound"), "input").unwrap();
    let spec = root.join("spec.json");
    fs::write(&spec, serde_json::to_vec(&json!({"schema":1,"source_root":source,"inputs":supervisor::bind_tree(&source).unwrap(),"cases":[{"id":"exact","execution":{"kind":"argv","argv":["sh","-c",command],"env":{}},"deadline_seconds":10,"artifacts":[],"dependencies":[],"resources":[],"platform":"linux"}]})).unwrap()).unwrap();
    (spec, root.join("runs"))
}

fn rust_fixture(root: &Path) -> PathBuf {
    let source = repo(root);
    fs::create_dir(source.join("src")).unwrap();
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nname = 'native-fixture'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(
        source.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = 'native-fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    fs::write(source.join("src/lib.rs"), "#[test] fn native_pass() { assert_eq!(2 + 2, 4); }\n#[test] #[ignore] fn native_ignored() {}\n#[test] fn native_fail() { panic!(\"intentional fixture failure\"); }\n").unwrap();
    source
}

fn argv_suite(root: &Path, id: &str, argv: &[&str]) -> PathBuf {
    let path = suite(root, &[(id, "fixture.Checks.test")], true);
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, original.replace("execution = {kind = 'argv', argv = ['python3', '-B', '-m', 'unittest', 'fixture.Checks.test']}\npython_migration_id = 'fixture.Checks.test'", &format!("execution = {{kind = 'argv', argv = {}}}", serde_json::to_string(argv).unwrap()))).unwrap();
    path
}

#[test]
fn executor_dispatch_uses_real_cargo_and_rejects_zero_ignored_and_failure() {
    let temp = tempfile::tempdir().unwrap();
    let source = rust_fixture(temp.path());
    for (selector, passed) in [
        ("native_pass", true),
        ("not_a_test", false),
        ("native_ignored", false),
        ("native_fail", false),
    ] {
        let workspace = temp.path().join(selector);
        fs::create_dir(&workspace).unwrap();
        let spec = ExecutorSpec {
            schema: 1,
            case_id: "native-fixture".into(),
            argv: [
                "cargo", "test", "--locked", "--lib", selector, "--", "--exact",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            env: [(
                "CARGO_TARGET_DIR".into(),
                temp.path().join("build").to_string_lossy().into_owned(),
            )]
            .into(),
            workspace: workspace.clone(),
            selector: Some(selector.into()),
            prerequisite: None,
        };
        let path = workspace.join("executor.json");
        fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
            .args(["execute", "--spec", text(&path)])
            .current_dir(&source)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            passed,
            "{selector}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(workspace.join("acceptance.json").exists(), passed);
        if passed {
            let artifact: Value =
                serde_json::from_slice(&fs::read(workspace.join("acceptance.json")).unwrap())
                    .unwrap();
            assert_eq!(
                artifact["assertions"][0]["name"],
                "Rust harness executed native_pass"
            );
            assert!(workspace.join("log-bindings.json").exists());
        }
    }
}

#[test]
fn catalog_native_executor_binds_retained_binary_and_spec() {
    let temp = tempfile::tempdir().unwrap();
    let source = rust_fixture(temp.path());
    let catalog = argv_suite(
        temp.path(),
        "rust.native_pass",
        &[
            "cargo",
            "test",
            "--locked",
            "--lib",
            "native_pass",
            "--",
            "--exact",
        ],
    );
    let base = temp.path().join("runs");
    let output = cli(&[
        "run",
        "--suite",
        text(&catalog),
        "--source",
        text(&source),
        "--base",
        text(&base),
        "--id",
        "native",
        "--foreground",
    ]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let directory = base.join("native");
    let spec: supervisor::RunSpec =
        serde_json::from_slice(&fs::read(directory.join("spec.json")).unwrap()).unwrap();
    let retained_binary = base.join("native-artifacts/harbor-db-test");
    assert!(
        spec.inputs
            .iter()
            .any(|binding| binding.path == retained_binary)
    );
    let adapter = spec
        .inputs
        .iter()
        .find(|binding| {
            binding
                .path
                .file_name()
                .is_some_and(|name| name == "executor.json")
        })
        .unwrap();
    assert_eq!(
        supervisor::bind_file(&adapter.path).unwrap().sha256,
        adapter.sha256
    );
    let executor: ExecutorSpec = serde_json::from_slice(&fs::read(&adapter.path).unwrap()).unwrap();
    assert_eq!(executor.selector.as_deref(), Some("native_pass"));
    assert_eq!(runner::verify(&directory).unwrap().verdict, Verdict::Passed);
    fs::write(&adapter.path, "{}").unwrap();
    assert_ne!(runner::verify(&directory).unwrap().verdict, Verdict::Passed);
}

#[test]
fn unregistered_native_shapes_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let source = rust_fixture(temp.path());
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_harbor-db-test"))
        .canonicalize()
        .unwrap();
    for (index, argv) in [
        vec!["cargo", "test", "native_pass"],
        vec![
            "cargo",
            "test",
            "native_ignored",
            "--",
            "--exact",
            "--ignored",
        ],
        vec!["cargo", "build", "native_pass", "--", "--exact"],
    ]
    .iter()
    .enumerate()
    {
        let catalog = argv_suite(temp.path(), "rust.shape", argv);
        let error = runner::create_with_executor(
            &catalog,
            &source,
            &temp.path().join("runs"),
            &format!("shape-{index}"),
            Profile::Fast,
            Some(&executable),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unregistered Cargo shape"));
    }
}

#[test]
fn prerequisite_executor_validates_inventory_and_major() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("package");
    fs::create_dir_all(package.join("bin")).unwrap();
    for name in [
        "initdb",
        "pg_ctl",
        "psql",
        "pg_dump",
        "pg_restore",
        "postgres",
        "pg_upgrade",
    ] {
        let path = package.join("bin").join(name);
        fs::write(
            &path,
            if name == "postgres" {
                "#!/bin/sh\nprintf 'postgres (PostgreSQL) 18.0\\n'\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            },
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // Exercise prerequisite metadata binding and the worker adapter, not only direct dispatch.
    let source = repo(temp.path());
    let catalog = argv_suite(temp.path(), "prerequisite.disposable-postgres", &["true"]);
    let original = fs::read_to_string(&catalog).unwrap();
    fs::write(
        &catalog,
        original.replace(
            "argv = [\"true\"]}",
            &format!(
                "argv = [\"true\"], env = {{HARBOR_DB_TEST_POSTGRES = {}}}}}",
                serde_json::to_string(text(&package)).unwrap()
            ),
        ),
    )
    .unwrap();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_harbor-db-test"))
        .canonicalize()
        .unwrap();
    let directory = runner::create_with_executor(
        &catalog,
        &source,
        &temp.path().join("runs"),
        "prerequisite",
        Profile::Fast,
        Some(&executable),
    )
    .unwrap();
    assert!(cli(&["worker", text(&directory)]).status.success());
    let spec: supervisor::RunSpec =
        serde_json::from_slice(&fs::read(directory.join("spec.json")).unwrap()).unwrap();
    let binding = spec
        .inputs
        .iter()
        .find(|input| input.path.file_name().is_some_and(|n| n == "executor.json"))
        .unwrap();
    let adapter: ExecutorSpec = serde_json::from_slice(&fs::read(&binding.path).unwrap()).unwrap();
    assert_eq!(adapter.prerequisite, Some((package.clone(), 18)));
    assert_eq!(runner::verify(&directory).unwrap().verdict, Verdict::Passed);
    for (label, major, passed) in [
        ("inventory", 18, true),
        ("wrong-major", 17, false),
        ("missing-tool", 18, false),
    ] {
        if label == "missing-tool" {
            fs::remove_file(package.join("bin/pg_upgrade")).unwrap();
        }
        let workspace = temp.path().join(label);
        fs::create_dir(&workspace).unwrap();
        let spec = ExecutorSpec {
            schema: 1,
            case_id: label.into(),
            argv: vec!["true".into()],
            env: Default::default(),
            workspace: workspace.clone(),
            selector: None,
            prerequisite: Some((package.clone(), major)),
        };
        let path = workspace.join("executor.json");
        fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
        let output = cli(&["execute", "--spec", text(&path)]);
        assert_eq!(
            output.status.success(),
            passed,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(workspace.join("acceptance.json").exists(), passed);
    }
}

#[test]
fn exit_zero_cannot_qualify_and_worker_observe_dispatch_are_real() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, base) = explicit(temp.path(), "exit 0");
    let created = cli(&[
        "create",
        "--spec",
        text(&spec),
        "--base",
        text(&base),
        "--id",
        "dispatch",
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let directory = base.join("dispatch");
    assert_eq!(cli(&["worker", text(&directory)]).status.code(), Some(2));
    assert!(
        cli(&["observe", text(&directory), "--once"])
            .status
            .success()
    );
    let verified = cli(&["verify", text(&directory)]);
    assert_eq!(verified.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&verified.stdout).unwrap()["verdict"],
        "failed"
    );
    assert!(!cli(&["worker", text(&directory)]).status.success());
}

#[test]
fn actual_unittest_result_passes_but_missing_coverage_never_does() {
    let temp = tempfile::tempdir().unwrap();
    let source = repo(temp.path());
    fs::create_dir(source.join("tests")).unwrap();
    fs::write(source.join("tests/test_fixture.py"), "import unittest\nclass Checks(unittest.TestCase):\n def test_pass(self): self.assertEqual(2 + 2, 4)\n").unwrap();
    let catalog = suite(
        temp.path(),
        &[("python.fixture", "test_fixture.Checks.test_pass")],
        false,
    );
    let base = temp.path().join("runs");
    let directory = runner::create(&catalog, &source, &base, "coverage", Profile::Fast).unwrap();
    // Execute the retained source even after the working checkout changes.
    fs::write(
        source.join("tests/test_fixture.py"),
        "raise RuntimeError('live checkout')",
    )
    .unwrap();
    assert_eq!(cli(&["worker", text(&directory)]).status.code(), Some(2));
    assert_eq!(
        supervisor::verify(&directory).unwrap().verdict,
        Verdict::Passed
    );
    let verdict = runner::verify(&directory).unwrap();
    assert_eq!(verdict.verdict, Verdict::Incomplete);
    assert_eq!(
        verdict
            .reasons
            .iter()
            .filter(|r| r.starts_with("coverage "))
            .count(),
        7
    );
    assert!(
        cli(&["observe", text(&directory), "--once"])
            .status
            .success()
    );
    let notification: Value = serde_json::from_slice(
        &fs::read(directory.join("qualification-notification.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(notification["details"]["verdict"], "incomplete");
    let supervisor_notification: Value =
        serde_json::from_slice(&fs::read(directory.join("terminal-notification.json")).unwrap())
            .unwrap();
    assert_eq!(
        supervisor_notification["event"],
        "runner_owns_qualification_notification"
    );
    let status = cli(&["status", text(&directory)]);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["verification"]["verdict"],
        "incomplete"
    );
}

#[test]
fn covered_unittest_foreground_run_qualifies_and_retains_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let source = repo(temp.path());
    fs::create_dir(source.join("tests")).unwrap();
    fs::write(source.join("tests/test_fixture.py"), "import unittest\nclass Checks(unittest.TestCase):\n def test_pass(self): self.assertTrue(True)\n").unwrap();
    let catalog = suite(
        temp.path(),
        &[("python.fixture", "test_fixture.Checks.test_pass")],
        true,
    );
    let base = temp.path().join("runs");
    let output = cli(&[
        "run",
        "--suite",
        text(&catalog),
        "--source",
        text(&source),
        "--base",
        text(&base),
        "--id",
        "passing",
        "--foreground",
    ]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let directory = base.join("passing");
    assert_eq!(runner::verify(&directory).unwrap().verdict, Verdict::Passed);
    fs::write(base.join("passing-artifacts/unittest-v1.py"), "changed").unwrap();
    assert_ne!(runner::verify(&directory).unwrap().verdict, Verdict::Passed);
}

#[test]
fn skipped_and_failed_tests_are_never_semantic_success() {
    let temp = tempfile::tempdir().unwrap();
    let source = repo(temp.path());
    fs::create_dir(source.join("tests")).unwrap();
    fs::write(source.join("tests/test_fixture.py"), "import unittest\nclass Checks(unittest.TestCase):\n @unittest.skip('fixture')\n def test_skip(self): pass\n def test_fail(self): self.fail('honest failure')\n").unwrap();
    let catalog = suite(
        temp.path(),
        &[
            ("python.skip", "test_fixture.Checks.test_skip"),
            ("python.fail", "test_fixture.Checks.test_fail"),
        ],
        true,
    );
    let directory = runner::create(
        &catalog,
        &source,
        &temp.path().join("runs"),
        "failure",
        Profile::Fast,
    )
    .unwrap();
    assert_eq!(cli(&["worker", text(&directory)]).status.code(), Some(2));
    let status = runner::status(&directory).unwrap();
    assert_eq!(status.verification.verdict, Verdict::Failed);
    assert!(
        status
            .results
            .iter()
            .all(|r| r.code == Some(1) && !r.evidence_errors.is_empty())
    );
}

#[test]
fn identities_are_safe_and_dependency_order_survives_selection() {
    assert_ne!(runner::safe_identity("a.b"), runner::safe_identity("a-b"));
    let temp = tempfile::tempdir().unwrap();
    let source = repo(temp.path());
    let catalog = suite(
        temp.path(),
        &[
            ("dependent.dot", "fixture.Check.test"),
            ("first.dot", "fixture.Check.first"),
        ],
        true,
    );
    let original = fs::read_to_string(&catalog).unwrap();
    fs::write(
        &catalog,
        original.replacen("depends_on = []", "depends_on = ['first.dot']", 1),
    )
    .unwrap();
    let directory = runner::create(
        &catalog,
        &source,
        &temp.path().join("runs"),
        "ordering",
        Profile::Fast,
    )
    .unwrap();
    let spec: supervisor::RunSpec =
        serde_json::from_slice(&fs::read(directory.join("spec.json")).unwrap()).unwrap();
    assert_eq!(spec.cases[0].id, runner::safe_identity("first.dot"));
    assert_eq!(spec.cases[1].dependencies, vec![spec.cases[0].id.clone()]);
    assert!(
        spec.cases
            .iter()
            .all(|c| c.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    );
    assert!(
        runner::create(
            &catalog,
            &source,
            &source.join("state"),
            "inside",
            Profile::Fast
        )
        .is_err()
    );
}

#[test]
fn watch_disconnect_and_time_limit_do_not_cancel_work() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, base) = explicit(temp.path(), "sleep 1");
    assert!(
        cli(&[
            "create",
            "--spec",
            text(&spec),
            "--base",
            text(&base),
            "--id",
            "watching"
        ])
        .status
        .success()
    );
    let directory = base.join("watching");
    let mut worker = Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .args(["worker", text(&directory)])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let watched = cli(&["watch", text(&directory), "--seconds", "0"]);
    assert!(watched.status.success());
    assert!(serde_json::from_slice::<Value>(&watched.stdout).is_ok());
    let mut watcher = Command::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .args(["watch", text(&directory), "--interval-ms", "10"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(watcher.stdout.take());
    assert!(watcher.wait().unwrap().success());
    assert!(!directory.join("cancel.json").exists());
    assert_eq!(worker.wait().unwrap().code(), Some(2));
    assert!(directory.join("terminal.json").exists());
}

#[test]
fn cancellation_is_a_durable_request_consumed_by_worker() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, base) = explicit(temp.path(), "exit 0");
    assert!(
        cli(&[
            "create",
            "--spec",
            text(&spec),
            "--base",
            text(&base),
            "--id",
            "cancelled"
        ])
        .status
        .success()
    );
    let directory = base.join("cancelled");
    assert!(cli(&["cancel", text(&directory)]).status.success());
    assert_eq!(cli(&["worker", text(&directory)]).status.code(), Some(2));
    assert_eq!(
        supervisor::status(&directory).unwrap().results[0].reason,
        supervisor::ExitReason::Cancelled
    );
}
