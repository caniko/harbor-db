#![cfg(unix)]

use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    manifest: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("owned test directory");
        let manifest = directory.path().join("plan with spaces.json");
        Self {
            directory,
            manifest,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn write_plan(&self, operations: Vec<Value>) {
        fs::write(
            &self.manifest,
            serde_json::to_vec(&json!({
                "version": 1,
                "name": "CLI contract",
                "operations": operations,
            }))
            .expect("serialize manifest"),
        )
        .expect("write manifest");
    }

    fn spawn(&self, mode: &str, args: &[&str]) -> OwnedProcess {
        self.spawn_manifest(&self.manifest, mode, args)
    }

    fn spawn_manifest(&self, manifest: &Path, mode: &str, args: &[&str]) -> OwnedProcess {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harbor-db"));
        command
            .arg(mode)
            .arg("--manifest")
            .arg(manifest)
            .args(args)
            .env("ROOT", self.directory.path())
            .env(
                "CREDENTIALS_DIRECTORY",
                self.path("credential directory with spaces"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe, touches no Rust state, and gives
        // this child and its commands a test-owned process group for cleanup.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        OwnedProcess {
            child: Some(command.spawn().expect("start actual harbor-db binary")),
        }
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        self.spawn(mode, args).finish()
    }
}

struct OwnedProcess {
    child: Option<Child>,
}

impl OwnedProcess {
    fn kill_group(&mut self) {
        let child = self.child.as_ref().expect("owned unreaped child");
        let group = i32::try_from(child.id()).expect("process group fits pid_t");
        // SAFETY: this unreaped child established its own session via
        // setsid; the negative PID targets only its owned process group.
        assert_eq!(unsafe { libc::kill(-group, libc::SIGKILL) }, 0);
    }

    fn finish(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(20);
        let child = self.child.as_mut().expect("owned child");
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait for CLI") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI exceeded owned test deadline"
            );
            thread::sleep(Duration::from_millis(10));
        };
        // try_wait reaped the leader. Remove it from the cleanup guard before
        // any fallible reads, so cleanup never signals a released process ID.
        let mut child = self.child.take().expect("completed child");
        assert_eq!(child.wait().expect("cached CLI exit status"), status);
        // Output is deliberately bounded in these fixtures; read it after exit
        // so assertion failures include both streams without reader threads.
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout pipe")
            .read_to_end(&mut stdout)
            .expect("read CLI stdout");
        child
            .stderr
            .take()
            .expect("stderr pipe")
            .read_to_end(&mut stderr)
            .expect("read CLI stderr");
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let group = i32::try_from(child.id()).expect("process group fits pid_t");
            // SAFETY: while retained here the child is not released/reused;
            // kill the entire test-owned group before reaping its leader.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

fn shell(body: &str) -> Value {
    json!({"program": "sh", "args": ["-c", body]})
}

fn operation(id: &str, apply: &str, check: &str) -> Value {
    json!({"id": id, "apply": shell(apply), "check": shell(check)})
}

fn assert_output(output: &Output, code: i32, stdout: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        output.stdout,
        stdout.as_bytes(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_error(output: &Output, message: &str) {
    assert_output(output, 1, "");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "expected {message:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn legacy_json_toml_typedb_and_credential_argument_bytes() {
    let fixture = Fixture::new();
    let credential_directory = fixture.path("credential directory with spaces");
    fs::create_dir(&credential_directory).expect("credential directory");
    // Paths only: no credential contents are needed or read by this fixture.
    let mut schema = operation("schema", "exit 0", "exit 0");
    schema["backend"] = json!("typedb");
    schema["apply"] = json!({
        "program": "sh",
        "args": ["-c", "printf '%s\\0' \"$1\" \"$2\" \"$PASSWORD_FILE\" > \"$ROOT/argv\"", "fixture", "literal argument with spaces"],
        "credential_args": ["typedb-password"],
        "credential_environment": {"PASSWORD_FILE": "typedb-password"},
    });
    fixture.write_plan(vec![schema]);
    assert_output(
        &fixture.run("validate", &[]),
        0,
        "database-operation plan is valid\n",
    );
    assert!(
        !fixture.path("argv").exists(),
        "validation must not invoke commands"
    );
    assert_output(&fixture.run("apply", &[]), 0, "schema: applied\n");
    let credential_path = credential_directory.join("typedb-password");
    let mut expected = b"literal argument with spaces\0".to_vec();
    for _ in 0..2 {
        expected.extend_from_slice(credential_path.as_os_str().as_encoded_bytes());
        expected.push(0);
    }
    assert_eq!(
        fs::read(fixture.path("argv")).expect("captured argv bytes"),
        expected
    );
    assert!(
        !credential_path.exists(),
        "planner passes references without reading secrets"
    );

    let toml_manifest = fixture.path("legacy plan.toml");
    fs::write(
        &toml_manifest,
        "version = 1\nname = 'legacy'\n[[operations]]\nid = 'schema'\nbackend = 'typedb'\n[operations.apply]\nprogram = 'sh'\nargs = ['-c', 'exit 0']\n[operations.check]\nprogram = 'sh'\nargs = ['-c', 'exit 0']\n",
    )
    .expect("legacy TOML fixture");
    assert_output(
        &fixture
            .spawn_manifest(&toml_manifest, "validate", &[])
            .finish(),
        0,
        "database-operation plan is valid\n",
    );
    assert_output(
        &fixture
            .spawn_manifest(&toml_manifest, "check", &[])
            .finish(),
        0,
        "schema: current\n",
    );
}

#[test]
fn parser_validation_and_runtime_failures_keep_public_exit_codes() {
    let fixture = Fixture::new();
    fixture.write_plan(vec![operation("failure", "exit 17", "exit 7")]);
    assert_error(
        &fixture.run("apply", &[]),
        "failure command exited with status 17",
    );
    assert_error(
        &fixture.run("check", &[]),
        "failure command exited with status 7",
    );
    assert_error(
        &fixture.run("restore", &[]),
        "failure command exited with status 7",
    );
    let parser = fixture.run("validate", &["--operation", "failure"]);
    assert_output(&parser, 2, "");
    assert!(String::from_utf8_lossy(&parser.stderr).contains("unexpected argument '--operation'"));
    let parser = fixture.run("unknown", &[]);
    assert_output(&parser, 2, "");
    assert!(String::from_utf8_lossy(&parser.stderr).contains("unrecognized subcommand"));

    fs::write(
        &fixture.manifest,
        br#"{"version":2,"name":"future","operations":[]}"#,
    )
    .expect("future manifest");
    assert_error(
        &fixture.run("validate", &[]),
        "unsupported migration plan version 2",
    );
    fs::write(&fixture.manifest, b"not JSON").expect("malformed manifest");
    assert_error(&fixture.run("validate", &[]), "decode migration plan");
}

#[test]
fn apply_exact_selection_runs_dependencies_once_in_declared_order() {
    let fixture = Fixture::new();
    let mut child = operation("child", "printf 'child\\n' >> \"$ROOT/actions\"", "exit 0");
    child["depends_on"] = json!(["parent"]);
    fixture.write_plan(vec![
        child,
        operation(
            "unselected",
            "printf 'wrong\\n' >> \"$ROOT/actions\"",
            "exit 0",
        ),
        operation(
            "parent",
            "printf 'parent\\n' >> \"$ROOT/actions\"",
            "exit 0",
        ),
    ]);
    assert_error(
        &fixture.run("apply", &["--operation", "missing"]),
        "selected migration operation does not exist: missing",
    );
    assert!(!fixture.path("actions").exists());
    assert_output(
        &fixture.run("apply", &["--operation", "child", "--operation", "child"]),
        0,
        "parent: applied\nchild: applied\n",
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("action order"),
        b"parent\nchild\n"
    );
    assert_output(
        &fixture.run("check", &["--operation", "child"]),
        0,
        "parent: current\nchild: current\n",
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("read-only check"),
        b"parent\nchild\n"
    );
}

#[test]
fn operator_restore_requires_exact_opt_in_and_confirmation_before_writes() {
    let fixture = Fixture::new();
    let mut manual = operation(
        "manual",
        "printf 'manual\\n' >> \"$ROOT/actions\"; touch \"$ROOT/manual\"",
        "test -e \"$ROOT/manual\" || exit 2",
    );
    manual["safety"] = json!("operator_confirmed");
    manual["depends_on"] = json!(["parent"]);
    let parent = operation(
        "parent",
        "printf 'parent\\n' >> \"$ROOT/actions\"; touch \"$ROOT/parent\"",
        "test -e \"$ROOT/parent\" || exit 2",
    );
    fixture.write_plan(vec![parent, manual]);
    assert_output(
        &fixture.run("check", &[]),
        2,
        "parent: pending\nmanual: pending\n",
    );
    assert!(!fixture.path("actions").exists());
    for mode in ["apply", "restore"] {
        assert_error(
            &fixture.run(mode, &["--operation", "manual"]),
            "operation manual requires explicit confirmation",
        );
        assert!(
            !fixture.path("actions").exists(),
            "confirmation is checked before dependencies write"
        );
    }
    assert_output(
        &fixture.run("restore", &[]),
        0,
        "parent: restored\nmanual: skipped-manual\n",
    );
    assert!(!fixture.path("manual").exists());
    // Confirmation alone is not selection, and selecting a dependency does
    // not opt in to the dependent operator-only operation.
    assert_output(
        &fixture.run("restore", &["--confirm"]),
        0,
        "parent: current\nmanual: skipped-manual\n",
    );
    assert_output(
        &fixture.run("restore", &["--operation", "parent", "--confirm"]),
        0,
        "parent: current\n",
    );
    assert!(!fixture.path("manual").exists());
    assert_output(
        &fixture.run("restore", &["--operation", "manual", "--confirm"]),
        0,
        "parent: current\nmanual: restored\n",
    );
    assert_output(
        &fixture.run("restore", &["--operation", "manual", "--confirm"]),
        0,
        "parent: current\nmanual: current\n",
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("only pending actions"),
        b"parent\nmanual\n"
    );
}

#[test]
fn restore_converges_repeatedly_and_reports_pending_or_missing_checks() {
    let fixture = Fixture::new();
    fixture.write_plan(vec![operation(
        "resource",
        "printf 'repair\\n' >> \"$ROOT/actions\"; printf 'ready' > \"$ROOT/state\"",
        "test -e \"$ROOT/state\" || exit 2",
    )]);
    assert_output(&fixture.run("check", &[]), 2, "resource: pending\n");
    assert!(!fixture.path("state").exists());
    assert_output(&fixture.run("restore", &[]), 0, "resource: restored\n");
    for _ in 0..2 {
        assert_output(&fixture.run("restore", &[]), 0, "resource: current\n");
        assert_output(&fixture.run("check", &[]), 0, "resource: current\n");
    }
    assert_eq!(
        fs::read(fixture.path("state")).expect("converged state"),
        b"ready"
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("single repair"),
        b"repair\n"
    );

    fixture.write_plan(vec![operation("stuck", "exit 0", "exit 2")]);
    assert_output(&fixture.run("restore", &[]), 2, "stuck: pending\n");
    fixture.write_plan(vec![json!({
        "id": "apply-only",
        "apply": shell("printf 'wrong' >> \"$ROOT/actions\""),
    })]);
    for mode in ["check", "restore"] {
        assert_error(
            &fixture.run(mode, &[]),
            "operation apply-only has no check command",
        );
    }
    assert_eq!(
        fs::read(fixture.path("actions")).expect("missing checks never apply"),
        b"repair\n"
    );
}

#[test]
fn concurrent_read_only_checks_preserve_preexisting_state() {
    let fixture = Fixture::new();
    fs::write(fixture.path("state"), b"acknowledged state").expect("preexisting state");
    fs::write(fixture.path("actions"), b"preexisting actions\n").expect("preexisting actions");
    fixture.write_plan(vec![operation(
        "resource",
        "printf 'wrong' >> \"$ROOT/actions\"",
        "sleep 1; test -e \"$ROOT/state\"",
    )]);
    let processes: Vec<_> = (0..3).map(|_| fixture.spawn("check", &[])).collect();
    for process in processes {
        assert_output(&process.finish(), 0, "resource: current\n");
    }
    assert_eq!(
        fs::read(fixture.path("state")).expect("unchanged state"),
        b"acknowledged state"
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("unchanged actions"),
        b"preexisting actions\n"
    );
}

#[test]
fn interrupted_restore_preserves_acknowledged_state_and_explicit_rerun_converges() {
    let fixture = Fixture::new();
    fs::write(fixture.path("preexisting"), b"acknowledged before CLI").expect("preexisting state");
    fs::write(fixture.path("block"), b"block second command").expect("interruption gate");
    let first = operation(
        "first",
        "printf 'first\\n' >> \"$ROOT/actions\"; printf 'ready' > \"$ROOT/first\"",
        "test -e \"$ROOT/first\" || exit 2",
    );
    let mut second = operation(
        "second",
        "if test -e \"$ROOT/block\"; then touch \"$ROOT/started\"; sleep 30; fi; printf 'second\\n' >> \"$ROOT/actions\"; printf 'ready' > \"$ROOT/second\"",
        "test -e \"$ROOT/second\" || exit 2",
    );
    second["depends_on"] = json!(["first"]);
    fixture.write_plan(vec![first, second]);
    let mut process = fixture.spawn("restore", &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.path("started").exists() {
        assert!(
            Instant::now() < deadline,
            "restore did not reach interruption gate"
        );
        thread::sleep(Duration::from_millis(10));
    }
    process.kill_group();
    let interrupted = process.finish();
    assert!(
        !interrupted.status.success(),
        "interrupted CLI must fail; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr),
    );
    assert_eq!(
        fs::read(fixture.path("first")).expect("completed first action"),
        b"ready"
    );
    assert_eq!(
        fs::read(fixture.path("preexisting")).expect("retained state"),
        b"acknowledged before CLI"
    );
    assert!(!fixture.path("second").exists());
    assert_output(
        &fixture.run("check", &[]),
        2,
        "first: current\nsecond: pending\n",
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("only acknowledged action"),
        b"first\n"
    );
    fs::remove_file(fixture.path("block")).expect("release owned interruption gate");
    assert_output(
        &fixture.run("restore", &[]),
        0,
        "first: current\nsecond: restored\n",
    );
    assert_output(
        &fixture.run("restore", &[]),
        0,
        "first: current\nsecond: current\n",
    );
    assert_eq!(
        fs::read(fixture.path("second")).expect("rerun converged"),
        b"ready"
    );
    assert_eq!(
        fs::read(fixture.path("actions")).expect("no replay of current actions"),
        b"first\nsecond\n"
    );
    assert_eq!(
        fs::read(fixture.path("preexisting")).expect("retained state after rerun"),
        b"acknowledged before CLI"
    );
}
