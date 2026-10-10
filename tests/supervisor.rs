#![cfg(feature = "testing")]
use harbor_db::storage::process;
use harbor_db::testing::supervisor::{self, CaseSpec, Execution, RunSpec, Verdict};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, Instant},
};
#[path = "fixtures/supervisor_worker.rs"]
mod fixture;

fn spec(root: &Path, script: &str, deadline: u64) -> RunSpec {
    let snapshot = root.join("snapshot");
    fs::create_dir_all(&snapshot).unwrap();
    let source = snapshot.join("source");
    fs::write(&source, b"bound source").unwrap();
    let assertion = root.join("assertion.json");
    let script = format!(
        "{script}; printf '%s' '{{\"schema\":1,\"case_id\":\"case\",\"assertions\":[{{\"name\":\"fixture completed\",\"passed\":true}}]}}' > '{}'",
        assertion.display()
    );
    RunSpec {
        schema: 1,
        source_root: snapshot,
        inputs: supervisor::bind_tree(source.parent().unwrap()).unwrap(),
        cases: vec![CaseSpec {
            id: "case".into(),
            execution: Execution::Argv {
                argv: vec!["sh".into(), "-c".into(), script],
                env: BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]),
            },
            deadline_seconds: deadline,
            artifacts: vec![harbor_db::testing::evidence::ArtifactSpec {
                source: "case".into(),
                path: assertion,
                kind: harbor_db::testing::evidence::ArtifactKind::Semantic,
                required: true,
                sha256: None,
            }],
            dependencies: vec![],
            resources: vec![],
            platform: "linux".into(),
        }],
    }
}

#[test]
fn eof_is_not_exit_and_follower_has_no_authority() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(
        Some(tmp.path()),
        "eof",
        spec(tmp.path(), "exec 1>&- 2>&-; sleep 1", 5),
    )
    .unwrap();
    let started = Instant::now();
    let dir = run.clone();
    let thread = std::thread::spawn(move || supervisor::worker(&dir).unwrap());
    supervisor::watch(
        &run,
        Duration::from_millis(30),
        Some(Duration::from_millis(100)),
    )
    .unwrap();
    thread.join().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
}

#[test]
fn timeout_and_cancel_preserve_execution_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(Some(tmp.path()), "timeout", spec(tmp.path(), "sleep 20", 1))
        .unwrap();
    supervisor::worker(&run).unwrap();
    assert_eq!(
        supervisor::status(&run).unwrap().results[0].reason,
        supervisor::ExitReason::Timeout
    );
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Failed);
    let run = supervisor::create_run(Some(tmp.path()), "cancel", spec(tmp.path(), "sleep 20", 30))
        .unwrap();
    supervisor::cancel(&run).unwrap();
    supervisor::worker(&run).unwrap();
    assert_eq!(
        supervisor::status(&run).unwrap().results[0].reason,
        supervisor::ExitReason::Cancelled
    );
}

#[test]
fn persistent_anchor_busy_and_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let run =
        supervisor::create_run(Some(tmp.path()), "busy", spec(tmp.path(), "true", 5)).unwrap();
    let _lease = harbor_db::storage::durable::lock(&run.join("worker.lock"), false, false).unwrap();
    assert!(supervisor::worker(&run).is_err());
    assert!(supervisor::create_run(Some(tmp.path()), "busy", spec(tmp.path(), "true", 5)).is_err());
    assert!(
        supervisor::create_run(Some(tmp.path()), "../escape", spec(tmp.path(), "true", 5)).is_err()
    );
}

#[test]
fn observer_reattaches_and_missing_worker_is_incomplete() {
    let tmp = tempfile::tempdir().unwrap();
    let run =
        supervisor::create_run(Some(tmp.path()), "reattach", spec(tmp.path(), "true", 5)).unwrap();
    supervisor::observe_once(&run).unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Incomplete
    );
    supervisor::worker(&run).unwrap();
    supervisor::observe_once(&run).unwrap();
    supervisor::observe_once(&run).unwrap();
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
    fs::remove_file(run.join("results.json")).unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Incomplete
    );
}

#[test]
fn argv_cases_exclude_ambient_variables_and_keep_the_retained_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let mut selection = spec(
        tmp.path(),
        "test -z \"${HARBOR_UNDECLARED_TEST+x}\" && test \"$HARBOR_DECLARED_TEST\" = retained || exit 1",
        5,
    );
    if let Execution::Argv { env, .. } = &mut selection.cases[0].execution {
        env.insert("HARBOR_DECLARED_TEST".into(), "retained".into());
        env.insert("PATH".into(), std::env::var("PATH").unwrap());
    }
    let run = supervisor::create_run(Some(tmp.path()), "environment", selection).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "fixture::worker_process",
            "--ignored",
            "--nocapture",
        ])
        .env("HARBOR_SUPERVISOR_FIXTURE_RUN", &run)
        .env("HARBOR_UNDECLARED_TEST", "ambient-fixture-value")
        .env("HARBOR_DECLARED_TEST", "unretained-value");
    assert!(
        process::spawn(&mut command)
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
}

#[test]
fn killed_worker_cannot_relaunch_and_child_retains_authority() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec(tmp.path(), "sleep 2", 5);
    spec.cases[0].resources.push("fixture-resource".into());
    let run = supervisor::create_run(Some(tmp.path()), "worker-loss", spec).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "fixture::worker_process",
            "--ignored",
            "--nocapture",
        ])
        .env("HARBOR_SUPERVISOR_FIXTURE_RUN", &run);
    let mut worker = process::spawn(&mut command).unwrap();
    let start = Instant::now();
    loop {
        if supervisor::status(&run)
            .unwrap()
            .started
            .is_some_and(|s| s.child.is_some())
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fixture did not register child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.kill().unwrap();
    worker.wait().unwrap();
    assert!(!supervisor::status(&run).unwrap().worker_alive);
    assert!(
        harbor_db::storage::durable::lock(&run.join("worker.lock"), false, false).is_err(),
        "child must inherit persistent authority"
    );
    assert!(
        harbor_db::storage::durable::lock(
            &tmp.path().join("resources/fixture-resource.lock"),
            false,
            false
        )
        .is_err(),
        "child must retain resource authority"
    );
    supervisor::observe_once(&run).unwrap();
    let liveness: serde_json::Value =
        serde_json::from_slice(&fs::read(run.join("worker-liveness.json")).unwrap()).unwrap();
    assert_eq!(liveness["alive"], false);
    assert_eq!(
        liveness["saved"],
        serde_json::to_value(supervisor::status(&run).unwrap().started.unwrap().worker).unwrap(),
        "diagnostics must preserve the authoritative saved registration"
    );
    assert!(
        liveness["observed"]["current"].is_object()
            || liveness["observed"]["error"]["stage"].is_string(),
        "failed decision must retain either the observed identity or its failing stage: {liveness}"
    );
    assert_eq!(
        supervisor::status(&run).unwrap().results[0].reason,
        supervisor::ExitReason::Interrupted
    );
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Incomplete
    );
    assert!(supervisor::worker(&run).is_err());
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        supervisor::worker(&run).is_err(),
        "expired child lease still must not authorize rerun"
    );
}

#[test]
fn foreground_error_stops_observer_without_disk_stop_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(Some(tmp.path()), "foreground", spec(tmp.path(), "true", 5))
        .unwrap();
    let _lease = harbor_db::storage::durable::lock(&run.join("worker.lock"), false, false).unwrap();
    let started = Instant::now();
    assert!(supervisor::foreground(&run).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!run.join("observer-stop.json").exists());
}

#[test]
fn failed_verdict_dominates_indeterminate_and_incomplete() {
    let tmp = tempfile::tempdir().unwrap();
    let mut run_spec = spec(tmp.path(), "exit 7", 5);
    let mut unsupported = run_spec.cases[0].clone();
    unsupported.id = "unsupported".into();
    unsupported.platform = "other-os".into();
    unsupported.artifacts.clear();
    run_spec.cases.push(unsupported);
    let run = supervisor::create_run(Some(tmp.path()), "precedence", run_spec).unwrap();
    supervisor::worker(&run).unwrap();
    fs::write(tmp.path().join("snapshot/source"), "changed").unwrap();
    let state = supervisor::status(&run).unwrap();
    assert_eq!(state.results[0].code, Some(7));
    assert_eq!(
        state.results[1].reason,
        supervisor::ExitReason::UnsupportedPlatform
    );
    assert_eq!(state.verification.verdict, Verdict::Failed);
}

#[test]
fn persisted_activity_distinguishes_quiet_logs_heartbeat_and_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(Some(tmp.path()), "activity", spec(tmp.path(), "sleep 2", 5))
        .unwrap();
    let worker_run = run.clone();
    let worker = std::thread::spawn(move || supervisor::worker(&worker_run).unwrap());
    let began = Instant::now();
    loop {
        if supervisor::status(&run)
            .unwrap()
            .started
            .is_some_and(|s| s.child.is_some())
        {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(10));
    }
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut started: serde_json::Value =
        serde_json::from_slice(&fs::read(run.join("started.json")).unwrap()).unwrap();
    started["unix_seconds"] = (time - 400).into();
    harbor_db::storage::durable::write_json(&run.join("started.json"), &started).unwrap();
    harbor_db::storage::durable::write_json(
        &run.join("heartbeat.json"),
        &serde_json::json!({"unix_seconds": time - 9}),
    )
    .unwrap();
    supervisor::observe_once(&run).unwrap();
    let activity: supervisor::Activity =
        serde_json::from_slice(&fs::read(run.join("activity.json")).unwrap()).unwrap();
    assert!(activity.quiet);
    assert!(activity.log_age_seconds >= 400);
    assert!(activity.semantic_progress_age_seconds < 2);
    assert!(
        activity
            .heartbeat_age_seconds
            .is_some_and(|age| (9..12).contains(&age))
    );
    let notification = fs::read(run.join("quiet-notification.json")).unwrap();
    supervisor::observe_once(&run).unwrap();
    assert_eq!(
        fs::read(run.join("quiet-notification.json")).unwrap(),
        notification,
        "reattach must not repeat a quiet notification"
    );
    worker.join().unwrap();
    supervisor::observe_once(&run).unwrap();
    assert!(run.join("terminal-notification.json").exists());
}

#[test]
fn heartbeat_advances_without_stdout_or_semantic_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(
        Some(tmp.path()),
        "heartbeat",
        spec(tmp.path(), "sleep 11", 20),
    )
    .unwrap();
    let worker_run = run.clone();
    let worker = std::thread::spawn(move || supervisor::worker(&worker_run).unwrap());
    let began = Instant::now();
    let mut first = None;
    loop {
        if let Ok(bytes) = fs::read(run.join("heartbeat.json")) {
            let heartbeat: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let time = heartbeat["unix_seconds"].as_u64().unwrap();
            let initial = *first.get_or_insert(time);
            if time > initial {
                assert_eq!(fs::metadata(run.join("case.stdout.log")).unwrap().len(), 0);
                assert!(!run.join("progress.json").exists());
                break;
            }
        }
        assert!(
            began.elapsed() < Duration::from_secs(13),
            "heartbeat did not advance"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    worker.join().unwrap();
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
}

#[test]
fn fresh_logs_and_heartbeat_cannot_hide_five_minutes_without_meaningful_progress() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(
        Some(tmp.path()),
        "stalled-progress",
        spec(tmp.path(), "printf 'still talking'; sleep 20", 30),
    )
    .unwrap();
    let worker_run = run.clone();
    let worker = std::thread::spawn(move || supervisor::worker(&worker_run).unwrap());
    let exercise = (|| -> supervisor::Result<_> {
        let began = Instant::now();
        while supervisor::status(&run)?
            .started
            .is_none_or(|s| s.child.is_none())
        {
            if began.elapsed() >= Duration::from_secs(5) {
                return Err(std::io::Error::other("fixture did not register child").into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let mut started: serde_json::Value =
            serde_json::from_slice(&fs::read(run.join("started.json"))?)?;
        started["unix_seconds"] = (time - 400).into();
        harbor_db::storage::durable::write_json(&run.join("started.json"), &started)?;
        let aged_file = fs::File::open(run.join("started.json"))?;
        aged_file.set_times(
            fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(time - 400)),
        )?;
        let metadata = |m: fs::Metadata| {
            serde_json::json!({"dev": m.dev(), "ino": m.ino(),
            "mtime": m.mtime(), "mtime_nsec": m.mtime_nsec()})
        };
        // Capture both identities immediately: set_times acts on an fd, whereas
        // activity later opens the pathname independently.
        let aged_readback = serde_json::json!({
            "requested_unix_seconds": time - 400,
            "wall_unix_seconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            "fd": metadata(aged_file.metadata()?),
            "pathname": metadata(fs::metadata(run.join("started.json"))?),
            "started": serde_json::from_slice::<serde_json::Value>(&fs::read(run.join("started.json"))?)?,
        });
        let pid = std::process::id();
        let leader_stat = fs::read_to_string(format!("/proc/{pid}/task/{pid}/stat"))?;
        let leader_start = leader_stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .ok_or_else(|| std::io::Error::other("missing thread-group-leader start time"))?
            .to_string();
        let registered_leader =
            started["worker"]["pid"] == pid && started["worker"]["start_time"] == leader_start;
        harbor_db::storage::durable::write_json(
            &run.join("heartbeat.json"),
            &serde_json::json!({"unix_seconds": time}),
        )?;
        fs::write(run.join("case.stdout.log"), "recent log activity")?;
        let state = supervisor::observe_once(&run)?;
        let activity: supervisor::Activity =
            serde_json::from_slice(&fs::read(run.join("activity.json"))?)?;
        let receipts = || {
            [
                "progress.json",
                "terminal.json",
                "interrupted.json",
                "interrupted-terminal.json",
                "worker-liveness.json",
            ]
            .into_iter()
            .map(|name| {
                let value = match fs::read(run.join(name)) {
                    Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                        .unwrap_or_else(|e| serde_json::json!({"read_error": e.to_string()})),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
                    Err(e) => serde_json::json!({"read_error": e.to_string()}),
                };
                (name.to_string(), value)
            })
            .collect::<BTreeMap<_, _>>()
        };
        let diagnostics = format!(
            "aged readback: {aged_readback}; group leader: {leader_stat}; state: {state:?}; activity: {activity:?}; receipts: {:?}",
            receipts()
        );
        let notification = fs::read(run.join("stalled-notification.json")).ok();
        let repeated_state = supervisor::observe_once(&run)?;
        let repeated_notification = fs::read(run.join("stalled-notification.json")).ok();
        harbor_db::storage::durable::write_json(
            &run.join("progress.json"),
            &serde_json::json!({"unix_seconds": time}),
        )?;
        let recovered_state = supervisor::observe_once(&run)?;
        let recovered_activity: supervisor::Activity =
            serde_json::from_slice(&fs::read(run.join("activity.json"))?)?;
        let diagnostics = format!(
            "{diagnostics}; repeated state: {repeated_state:?}; recovered state: {recovered_state:?}; recovered activity: {recovered_activity:?}; recovered receipts: {:?}",
            receipts()
        );
        let live_state = supervisor::status(&run)?;
        Ok((
            activity,
            notification,
            repeated_notification,
            recovered_activity,
            live_state,
            registered_leader,
            diagnostics,
        ))
    })();
    // Cleanup precedes outcome assertions, including exercise errors, so a failed
    // diagnostic never strands the real worker or its authority-owning child.
    let cancelled = supervisor::cancel(&run);
    let joined = worker.join();
    cancelled.unwrap();
    joined.unwrap();
    let (
        activity,
        notification,
        repeated_notification,
        recovered_activity,
        live_state,
        registered_leader,
        diagnostics,
    ) = exercise.unwrap();
    assert!(
        registered_leader,
        "worker registration must bind the thread-group leader; {diagnostics}"
    );
    assert!(!activity.quiet, "{diagnostics}");
    assert!(
        activity.stalled,
        "liveness and log chatter must not suppress the progress notification; {diagnostics}"
    );
    assert!(
        notification.is_some(),
        "missing stalled notification; {diagnostics}"
    );
    assert_eq!(repeated_notification, notification, "{diagnostics}");
    assert!(!recovered_activity.stalled, "{diagnostics}");
    assert!(
        !live_state.terminal,
        "progress recovery must leave execution nonterminal; {live_state:?}; {diagnostics}"
    );
    assert!(
        live_state.worker_alive,
        "notification must not terminate a live worker; {live_state:?}; {diagnostics}"
    );
}

#[test]
fn second_service_launch_failure_is_terminal_and_stops_only_own_observer() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(
        Some(tmp.path()),
        "launch-failure",
        spec(tmp.path(), "true", 5),
    )
    .unwrap();
    let stub = tmp.path().join("stub-bin");
    fs::create_dir(&stub).unwrap();
    let interpreter = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let log = tmp.path().join("service-commands.log");
    fs::write(stub.join("systemd-run"), format!("#!{}\nprintf 'launch %s\\n' \"$*\" >> \"$STUB_LOG\"\nfor arg in \"$@\"; do if [ \"$arg\" = worker ]; then exit 9; fi; done\nexit 0\n", interpreter.display())).unwrap();
    fs::write(
        stub.join("systemctl"),
        format!(
            "#!{}\nprintf 'stop %s\\n' \"$*\" >> \"$STUB_LOG\"\nexit 0\n",
            interpreter.display()
        ),
    )
    .unwrap();
    for name in ["systemd-run", "systemctl"] {
        fs::set_permissions(stub.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "fixture::detached_launch_failure",
            "--ignored",
            "--nocapture",
        ])
        .env("HARBOR_SUPERVISOR_FIXTURE_RUN", &run)
        .env("PATH", &stub)
        .env("STUB_LOG", &log);
    let child = process::spawn(&mut command).unwrap().wait().unwrap();
    assert!(child.success());
    let commands = fs::read_to_string(log).unwrap();
    assert_eq!(
        commands
            .lines()
            .filter(|l| l.starts_with("launch "))
            .count(),
        2,
        "rerun must not start services"
    );
    let stop = commands.lines().find(|l| l.starts_with("stop ")).unwrap();
    assert!(stop.contains("--user stop harbor-db-test-launch-failure-"));
    assert!(stop.ends_with("-observe"));
    assert!(!stop.contains("nix"));
    let failed = fs::read(run.join("launch-failed.json")).unwrap();
    let launcher = run.join("service-executable");
    assert!(
        launcher.is_file(),
        "services must retain executable bytes instead of a mutable target/debug path"
    );
    assert_eq!(
        fs::read(&launcher).unwrap(),
        fs::read(std::env::current_exe().unwrap()).unwrap()
    );
    assert!(
        commands
            .lines()
            .filter(|l| l.starts_with("launch "))
            .all(|l| l.contains(launcher.to_str().unwrap()))
    );
    let binding: supervisor::FileBinding =
        serde_json::from_slice(&fs::read(run.join("service-executable.json")).unwrap()).unwrap();
    assert_eq!(binding.path, launcher);
    assert_eq!(
        supervisor::bind_file(&binding.path).unwrap().sha256,
        binding.sha256
    );
    supervisor::observe_once(&run).unwrap();
    assert_eq!(
        fs::read(run.join("launch-failed.json")).unwrap(),
        failed,
        "observer attachment must preserve launch receipt"
    );
}

#[test]
fn changed_or_missing_detached_launcher_binding_cannot_qualify_retained_results() {
    let tmp = tempfile::tempdir().unwrap();
    let run = supervisor::create_run(
        Some(tmp.path()),
        "launcher-binding",
        spec(tmp.path(), "true", 5),
    )
    .unwrap();
    supervisor::worker(&run).unwrap();
    let executable = run.join("service-executable");
    fs::write(&executable, "retained launcher fixture").unwrap();
    let binding = supervisor::bind_file(&executable).unwrap();
    harbor_db::storage::durable::write_json(
        &run.join("service-executable.json"),
        &serde_json::to_value(&binding).unwrap(),
    )
    .unwrap();
    harbor_db::storage::durable::write_json(
        &run.join("launch-requested.json"),
        &serde_json::json!({"executable":binding}),
    )
    .unwrap();
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
    fs::write(&executable, "changed launcher bytes").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    fs::write(&executable, "retained launcher fixture").unwrap();
    fs::remove_file(run.join("service-executable.json")).unwrap();
    let verification = supervisor::verify(&run).unwrap();
    assert_eq!(verification.verdict, Verdict::Indeterminate);
    assert!(
        verification
            .reasons
            .iter()
            .any(|reason| reason.contains("binding missing"))
    );
}
