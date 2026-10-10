//! Packaged, testing-feature-only NixOS qualification executable, schema 1.
//! Cargo integration: `[[bin]] name="harbor-db-native-supervisor-fixture"`,
//! `path="tests/fixtures/native_supervisor.rs", required-features=["testing"]`.
//! No libtest/ignored-test adapter: assertions execute in the VM's user session.
use harbor_db::{
    storage::durable,
    testing::{
        evidence::{self, ArtifactKind, ArtifactSpec},
        supervisor::{self, CaseSpec, Execution, ExitReason, RunSpec, RunStatus, Verdict},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

type Result<T> = supervisor::Result<T>;

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}

fn wait(mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if predicate()? {
            return Ok(());
        }
        require(
            Instant::now() < deadline,
            "fixture checkpoint deadline exceeded",
        )?;
        thread::sleep(Duration::from_millis(50));
    }
}

fn private(path: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

// Every transport command has its own deadline. No shell parses fixture argv.
fn command(program: &Path, args: &[&str], success: bool) -> Result<(i32, Vec<u8>)> {
    let output = Command::new("timeout")
        .args(["--kill-after=2", "20"])
        .arg(program)
        .args(args)
        .output()?;
    let code = output.status.code().unwrap_or(-1);
    require(code != 124 && code != 137, "transport command timed out")?;
    if success {
        require(
            code == 0,
            &format!(
                "{} {:?}: {}",
                program.display(),
                args,
                String::from_utf8_lossy(&output.stderr)
            ),
        )?;
    }
    Ok((code, output.stdout))
}

fn ctl(args: &[&str]) -> Result<Vec<u8>> {
    let mut user = vec!["--user"];
    user.extend_from_slice(args);
    Ok(command(Path::new("systemctl"), &user, true)?.1)
}

fn property(unit: &str, name: &str) -> Result<String> {
    Ok(
        String::from_utf8(ctl(&["show", unit, "--property", name, "--value"])?)?
            .trim()
            .into(),
    )
}

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Run {
    path: PathBuf,
    units: Vec<String>,
    invoker: PathBuf,
    work: PathBuf,
}

impl Run {
    fn start(root: &Path, cli: &Path, id: &str, mode: &str, seconds: u64) -> Result<Self> {
        let work = root.join(format!("{id}-work"));
        private(&work)?;
        let source = root.join(format!("{id}-source"));
        private(&source)?;
        fs::write(source.join("fixture-version"), b"native-supervisor-v1\n")?;
        let payload = source.join("payload-executable");
        durable::atomic_write(
            &payload,
            &evidence::bounded_read(&std::env::current_exe()?)?,
        )?;
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o700))?;
        durable::open_regular(&payload, false)?.sync_all()?;
        let inputs = supervisor::bind_tree(&source)?;
        let spec = RunSpec {
            schema: 1,
            source_root: source,
            inputs,
            cases: vec![CaseSpec {
                id: id.into(),
                execution: Execution::Argv {
                    argv: vec![
                        payload.display().to_string(),
                        "payload".into(),
                        work.display().to_string(),
                        id.into(),
                        mode.into(),
                    ],
                    env: BTreeMap::new(),
                },
                deadline_seconds: seconds,
                artifacts: vec![ArtifactSpec {
                    source: id.into(),
                    path: work.join("acceptance.json"),
                    kind: ArtifactKind::Semantic,
                    required: true,
                    sha256: None,
                }],
                dependencies: vec![],
                resources: vec![],
                platform: "x86_64-linux".into(),
            }],
        };
        let spec_path = root.join(format!("{id}-spec.json"));
        fs::write(&spec_path, serde_json::to_vec(&spec)?)?;
        let invoker = root.join(format!("{id}-invoker"));
        fs::copy(cli, &invoker)?;
        fs::set_permissions(&invoker, fs::Permissions::from_mode(0o700))?;
        command(
            &invoker,
            &[
                "run",
                "--spec",
                spec_path.to_str().ok_or("spec path")?,
                "--base",
                root.to_str().ok_or("base path")?,
                "--id",
                id,
            ],
            true,
        )?;
        let path = root.join(id);
        let units: Vec<String> = serde_json::from_slice(&fs::read(path.join("units.json"))?)?;
        require(
            units.len() == 2,
            "detached launch must create observer and worker",
        )?;
        let run = Self {
            path,
            units,
            invoker,
            work,
        };
        wait(|| {
            Ok(supervisor::status(&run.path)?
                .started
                .is_some_and(|s| s.child.is_some()))
        })?;
        require(
            fs::metadata(&run.path)?.permissions().mode() & 0o777 == 0o700,
            "run must be private",
        )?;
        Ok(run)
    }

    fn release(&self) -> Result<()> {
        durable::atomic_write(&self.work.join("release"), b"release\n")?;
        Ok(())
    }

    fn terminal(&self) -> Result<RunStatus> {
        wait(|| Ok(supervisor::status(&self.path)?.terminal))?;
        supervisor::status(&self.path)
    }
}

fn payload(work: &Path, id: &str, mode: &str) -> Result<()> {
    // This is real native storage behavior, not a fabricated success exit:
    // an existing open descriptor pins old bytes across synchronized replacement.
    let state = work.join("state");
    durable::atomic_write(&state, b"before")?;
    let mut pinned = durable::open_regular(&state, false)?;
    durable::atomic_write(&state, b"after")?;
    let mut old = String::new();
    std::io::Read::read_to_string(&mut pinned, &mut old)?;
    require(
        old == "before",
        "pinned descriptor changed across atomic replacement",
    )?;
    require(fs::read(&state)? == b"after", "new publication not visible")?;
    durable::atomic_write(&work.join("ready"), b"ready")?;
    // Independent payload safety limit; normal cancellation/deadline is shorter.
    let limit = Instant::now() + Duration::from_secs(90);
    while !work.join("release").exists() {
        require(Instant::now() < limit, "payload safety deadline")?;
        thread::sleep(Duration::from_millis(50));
    }
    if mode != "missing" {
        let assertions = if mode == "empty" {
            vec![]
        } else {
            vec![
                json!({"name":"pinned descriptor preserves old inode", "passed":true}),
                json!({"name":"new path reads synchronized replacement", "passed":mode != "false"}),
            ]
        };
        durable::write_json(
            &work.join("acceptance.json"),
            &json!({"schema":1,"case_id":id,"assertions":assertions}),
        )?;
    }
    if mode == "nonzero" {
        return Err("deliberate payload failure after real assertions".into());
    }
    Ok(())
}

fn qualify(root: &Path, cli: &Path) -> Result<()> {
    private(root)?;
    let mut assertions = Vec::new();
    let mut runs = Vec::new();
    let run = Run::start(root, cli, "detached", "pass", 70)?;
    wait(|| Ok(run.work.join("ready").exists()))?;
    let initial = supervisor::status(&run.path)?
        .started
        .ok_or("registration missing")?;
    require(
        initial
            .worker
            .systemd_invocation
            .as_ref()
            .is_some_and(|v| !v.is_empty()),
        "worker has no systemd invocation",
    )?;
    require(
        property(&run.units[1], "MainPID")? == initial.worker.pid.to_string(),
        "worker unit PID mismatch",
    )?;
    require(
        property(&run.units[1], "InvocationID")?
            == initial
                .worker
                .systemd_invocation
                .clone()
                .unwrap_or_default(),
        "worker invocation mismatch",
    )?;
    require(
        property(&run.units[1], "Restart")? == "no",
        "worker restart policy must be disabled",
    )?;
    let frozen_spec = fs::read(run.path.join("spec.json"))?;
    let frozen_executable = supervisor::bind_file(&run.path.join("service-executable"))?.sha256;
    durable::atomic_write(&run.invoker, b"disposable invoking executable replaced\n")?;
    let mut watcher = Owned(
        Command::new("timeout")
            .args(["--kill-after=2", "15"])
            .arg(cli)
            .args([
                "watch",
                run.path.to_str().ok_or("run path")?,
                "--interval-ms",
                "10",
                "--seconds",
                "10",
            ])
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut output = BufReader::new(watcher.0.stdout.take().ok_or("watch stdout")?);
    let mut line = String::new();
    output.read_line(&mut line)?;
    require(
        serde_json::from_str::<Value>(&line)?["worker_alive"] == true,
        "watcher did not attach to live worker",
    )?;
    drop(output); // Real pipe EOF/broken pipe, not a cancellation request.
    wait(|| Ok(watcher.0.try_wait()?.is_some()))?;
    require(watcher.0.wait()?.success(), "broken-pipe watcher failed")?;
    let old_observer = property(&run.units[0], "InvocationID")?;
    ctl(&["restart", &run.units[0]])?;
    wait(|| Ok(property(&run.units[0], "InvocationID")? != old_observer))?;
    if run.path.join("observation.json").exists() {
        fs::remove_file(run.path.join("observation.json"))?;
    }
    // --collect intentionally retires an explicitly stopped transient unit.
    // Restart the still-existing service atomically to test reattachment.
    ctl(&["restart", &run.units[0]])?;
    wait(|| Ok(run.path.join("observation.json").exists()))?;
    let observed: RunStatus =
        serde_json::from_slice(&fs::read(run.path.join("observation.json"))?)?;
    require(
        serde_json::to_value(&observed.started)? == serde_json::to_value(Some(&initial))?,
        "restarted observer did not attach to original identities",
    )?;
    let restarted = property(&run.units[0], "InvocationID")?;
    ctl(&["kill", "--kill-whom=main", "--signal=KILL", &run.units[0]])?;
    wait(|| {
        let invocation = property(&run.units[0], "InvocationID")?;
        Ok(!invocation.is_empty()
            && invocation != restarted
            && property(&run.units[0], "ActiveState")? == "active")
    })?;
    let attached = supervisor::status(&run.path)?;
    require(
        attached.worker_alive,
        "observer/follower disconnect killed worker",
    )?;
    require(
        serde_json::to_value(&attached.started)? == serde_json::to_value(Some(&initial))?,
        "observer restart changed worker or child identity",
    )?;
    require(
        !run.path.join("cancel.json").exists(),
        "disconnect created cancellation",
    )?;
    require(
        fs::read(run.path.join("spec.json"))? == frozen_spec,
        "retained RunSpec changed",
    )?;
    require(
        supervisor::bind_file(&run.path.join("service-executable"))?.sha256 == frozen_executable,
        "retained executable changed",
    )?;
    require(
        property(&run.units[0], "ExecStart")?.contains(
            run.path
                .join("service-executable")
                .to_str()
                .ok_or("executable path")?,
        ),
        "observer restart did not use retained executable",
    )?;
    run.release()?;
    let final_state = run.terminal()?;
    require(
        final_state.verification.verdict == Verdict::Passed,
        "detached native assertions not accepted",
    )?;
    require(
        final_state.results.len() == 1 && final_state.results[0].artifacts.len() == 1,
        "nonzero acceptance receipt count required",
    )?;
    let receipt = &final_state.results[0].artifacts[0];
    require(
        read(&run.path.join(&receipt.retained_path))?["assertions"]
            .as_array()
            .is_some_and(|a| a.len() == 2),
        "native assertion count lost",
    )?;
    require(
        command(cli, &["verify", run.path.to_str().ok_or("run")?], true)?.0 == 0,
        "CLI positive verdict",
    )?;
    // Actual corrupted retained evidence must invalidate a formerly passing run.
    durable::atomic_write(&run.path.join(&receipt.retained_path), b"{}")?;
    require(
        supervisor::verify(&run.path)?.verdict != Verdict::Passed,
        "corrupted retained evidence accepted",
    )?;
    require(
        command(cli, &["verify", run.path.to_str().ok_or("run")?], false)?.0 == 2,
        "CLI tamper verdict must be nonpassing",
    )?;
    assertions.push("detached EOF, explicit observer restart, observer kill/restart, immutable executable, same worker/child, real native acceptance and tamper rejection");
    runs.push(json!({"run":run.path,"initial":initial,"accepted_before_tamper":final_state}));

    for mode in ["nonzero", "missing", "empty", "false", "cancel", "timeout"] {
        let run = Run::start(
            root,
            cli,
            mode,
            mode,
            if mode == "timeout" { 3 } else { 35 },
        )?;
        wait(|| Ok(run.work.join("ready").exists()))?;
        if mode == "cancel" {
            command(cli, &["cancel", run.path.to_str().ok_or("run")?], true)?;
            require(
                run.path.join("cancel.json").exists(),
                "durable cancel request absent",
            )?;
        } else if mode != "timeout" {
            run.release()?;
        }
        let terminal = run.terminal()?;
        require(
            terminal.verification.verdict == Verdict::Failed,
            "negative terminal run accepted",
        )?;
        let result = terminal.results.first().ok_or("negative result absent")?;
        if mode == "cancel" || mode == "timeout" {
            require(
                result.reason
                    == if mode == "cancel" {
                        ExitReason::Cancelled
                    } else {
                        ExitReason::Timeout
                    },
                "cancel/deadline reason lost",
            )?;
        } else if mode == "nonzero" {
            require(
                result.code.is_some_and(|c| c != 0) && result.artifacts.len() == 1,
                "nonzero result/evidence lost",
            )?;
        }
        require(
            command(cli, &["verify", run.path.to_str().ok_or("run")?], false)?.0 == 2,
            "CLI negative verdict must exit 2",
        )?;
        runs.push(json!({"run":run.path,"terminal":terminal}));
    }
    assertions.push("terminal nonzero exit, missing/empty/false semantic evidence, durable cancellation and hard deadline rejected with retained reasons");

    let run = Run::start(root, cli, "lost-worker", "pass", 60)?;
    wait(|| Ok(run.work.join("ready").exists()))?;
    ctl(&["stop", &run.units[0]])?;
    let mut started = supervisor::status(&run.path)?
        .started
        .ok_or("worker registration")?;
    let worker_identity = serde_json::to_value(&started.worker)?;
    ctl(&["kill", "--kill-whom=main", "--signal=KILL", &run.units[1]])?;
    wait(|| Ok(!supervisor::status(&run.path)?.worker_alive))?;
    // Deterministic stale-PID surrogate: saved child PID points to an unrelated
    // live process, with deliberately mismatching start time. Never signal it.
    let mut sentinel = Owned(Command::new("sleep").arg("60").spawn()?);
    let child = started.child.as_mut().ok_or("child registration")?;
    child.pid = sentinel.0.id();
    child.start_time = "0".into();
    durable::write_json(
        &run.path.join("started.json"),
        &serde_json::to_value(&started)?,
    )?;
    command(
        cli,
        &["observe", run.path.to_str().ok_or("run")?, "--once"],
        true,
    )?;
    let terminal = run.terminal()?;
    require(
        terminal.verification.verdict == Verdict::Incomplete,
        "lost worker must be incomplete",
    )?;
    require(
        terminal.results.len() == 1 && terminal.results[0].reason == ExitReason::Interrupted,
        "interrupted reason missing",
    )?;
    require(
        !run.path.join("results.json").exists() && !run.path.join("terminal.json").exists(),
        "observer invented worker completion",
    )?;
    require(
        serde_json::to_value(&terminal.started.as_ref().ok_or("started")?.worker)?
            == worker_identity,
        "observer replaced worker identity",
    )?;
    require(
        sentinel.0.try_wait()?.is_none(),
        "observer signalled unrelated saved PID",
    )?;
    require(
        command(cli, &["worker", run.path.to_str().ok_or("run")?], false)?.0 == 1,
        "second worker must be rejected",
    )?;
    command(
        cli,
        &["observe", run.path.to_str().ok_or("run")?, "--once"],
        true,
    )?;
    require(
        sentinel.0.try_wait()?.is_none(),
        "reattached observer signalled stale PID",
    )?;
    require(
        command(cli, &["verify", run.path.to_str().ok_or("run")?], false)?.0 == 2,
        "incomplete CLI verdict",
    )?;
    assertions.push("unexpected worker kill, attach/re-attach incomplete, duplicate worker rejection, deterministic stale child PID never signalled");
    runs.push(json!({"run":run.path,"terminal":terminal}));
    durable::write_json(
        &root.join("qualification.json"),
        &json!({
            "schema":1,"fixture":"native-supervisor-v1","full_suite_qualified":false,
            "assertions":assertions.iter().map(|name| json!({"name":name,"passed":true})).collect::<Vec<_>>(),
            "runs":runs,
        }),
    )?;
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let result = match args.get(1).and_then(|a| a.to_str()) {
        Some("qualify") if args.len() == 4 => qualify(Path::new(&args[2]), Path::new(&args[3])),
        Some("payload") if args.len() == 5 => payload(Path::new(&args[2]), &args[3].to_string_lossy(), &args[4].to_string_lossy()),
        _ => Err("usage: native-supervisor-fixture qualify ABSOLUTE_NEW_ROOT ABSOLUTE_CLI | payload WORK CASE MODE".into()),
    };
    if let Err(error) = result {
        eprintln!("native-supervisor-v1: {error}");
        std::process::exit(1);
    }
}
