use harbor_db::storage::observe::{
    ObservationReport, ObservationSnapshot, ObserveThresholds, evaluate,
};
use serde_json::{Value, json};

fn time(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(1_800_000_000 + seconds, 0)
        .unwrap()
        .to_rfc3339()
}

fn backend(pid: i32) -> Value {
    json!({
        "pid": pid, "backend_start": time(-20), "backend_type": "client backend",
        "datname": "postgres", "usename": "app", "application_name": "label",
        "query_id": 42, "xact_start": time(0), "query_start": time(0),
        "state_change": time(0), "state": "active", "is_commit": false,
        "blocking_pids": []
    })
}

fn observe(
    seconds: i64,
    epoch: i64,
    backends: &[Value],
    previous: Option<&ObservationReport>,
    thresholds: &ObserveThresholds,
) -> ObservationReport {
    evaluate(
        previous.map(|p| &p.state),
        &snapshot(seconds, epoch, backends),
        thresholds,
    )
    .unwrap()
}

fn snapshot(seconds: i64, epoch: i64, backends: &[Value]) -> ObservationSnapshot {
    serde_json::from_value(json!({
        "observed_at": time(seconds), "postmaster_start_time": time(epoch),
        "track_activities": true, "backends": backends
    }))
    .unwrap()
}

fn poll(
    seconds: i64,
    backends: &[Value],
    previous: Option<&ObservationReport>,
) -> ObservationReport {
    observe(
        seconds,
        -100,
        backends,
        previous,
        &ObserveThresholds::default(),
    )
}

fn output(report: &ObservationReport) -> Value {
    serde_json::to_value(report).unwrap()
}

fn findings(report: &ObservationReport, rule: &str) -> Vec<Value> {
    output(report)["open_findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == rule)
        .cloned()
        .collect()
}

#[test]
fn long_transaction_threshold_and_label_override() {
    let backend = backend(1);
    let rows = &[backend];
    for seconds in [-1, 899, 900] {
        assert!(findings(&poll(seconds, rows, None), "long-transaction").is_empty());
    }
    assert_eq!(
        findings(&poll(901, rows, None), "long-transaction").len(),
        1
    );
    let limits = ObserveThresholds {
        label_budgets: [("label".into(), 1200)].into(),
        ..ObserveThresholds::default()
    };
    assert!(
        findings(
            &observe(1200, -100, rows, None, &limits),
            "long-transaction"
        )
        .is_empty()
    );
    assert_eq!(
        findings(
            &observe(1201, -100, rows, None, &limits),
            "long-transaction"
        )
        .len(),
        1
    );
}

#[test]
fn explicit_commit_is_active_and_on_cpu() {
    let mut row = backend(1);
    row["is_commit"] = true.into();
    assert!(findings(&poll(300, &[row.clone()], None), "commit-phase").is_empty());
    let busy = poll(301, &[row.clone()], None);
    let finding = &findings(&busy, "commit-phase")[0];
    assert_eq!(finding["hint"], "deferred triggers");
    assert_eq!(finding["subject"]["kind"], "backend");
    assert!(!output(&busy).to_string().contains("\"query\":"));
    for field in ["wait_event_type", "wait_event"] {
        let mut waiting = row.clone();
        waiting[field] = "Timeout".into();
        assert!(findings(&poll(301, &[waiting], None), "commit-phase").is_empty());
    }
    row["is_commit"] = false.into();
    let implicit = poll(901, &[row], None);
    assert!(findings(&implicit, "commit-phase").is_empty());
    assert_eq!(findings(&implicit, "long-transaction").len(), 1);
}

#[test]
fn idle_transaction_states_and_boundary() {
    for state in [
        "idle in transaction",
        "idle in transaction (aborted)",
        "idle",
        "active",
    ] {
        let mut row = backend(1);
        row["state"] = state.into();
        let rows = &[row];
        assert!(findings(&poll(300, rows, None), "idle-in-transaction").is_empty());
        assert_eq!(
            findings(&poll(301, rows, None), "idle-in-transaction").len(),
            usize::from(state.starts_with("idle in transaction"))
        );
    }
}

#[test]
fn aborted_idle_episodes_with_no_xact_start_are_distinct() {
    let mut row = backend(1);
    row["state"] = "idle in transaction (aborted)".into();
    row["xact_start"] = Value::Null;
    let first = poll(301, &[row.clone()], None);
    let old = &findings(&first, "idle-in-transaction")[0];

    row["state_change"] = time(310).into();
    let second = poll(611, &[row], Some(&first));
    let new = &findings(&second, "idle-in-transaction")[0];
    assert_eq!(new["since"], time(611).replace("+00:00", "Z"));
    assert_eq!(
        output(&second)["transitions"],
        json!([{"kind": "resolved", "finding": old}, {"kind": "opened", "finding": new}])
    );
}

#[test]
fn unknown_samples_do_not_resolve_prior_findings() {
    let row = backend(1);
    let first = poll(1000, std::slice::from_ref(&row), None);
    let state = &first.state;
    let limits = ObserveThresholds::default();
    let mut disabled = row;
    disabled["state"] = "disabled".into();
    disabled["xact_start"] = Value::Null;
    let mut no_tracking = snapshot(1001, -100, &[]);
    no_tracking.track_activities = false;
    for sample in [no_tracking, snapshot(1001, -100, &[disabled])] {
        assert!(evaluate(Some(state), &sample, &limits).is_err());
    }
    let recovered = poll(1002, &[backend(1)], Some(&first));
    assert_eq!(output(&recovered)["transitions"], json!([]));
    assert_eq!(
        findings(&recovered, "long-transaction")[0]["since"],
        findings(&first, "long-transaction")[0]["since"]
    );
}

#[test]
fn disabled_worker_activity_does_not_resolve_prior_findings() {
    let mut worker = backend(1);
    worker["backend_type"] = "autovacuum worker".into();
    worker["wait_event_type"] = "Lock".into();
    let rows = &[worker.clone()];
    let initial = poll(10, rows, None);
    let open = poll(71, rows, Some(&initial));
    assert_eq!(findings(&open, "lock-wait").len(), 1);

    worker["state"] = "disabled".into();
    worker["query_start"] = Value::Null;
    let disabled = snapshot(72, -100, &[worker]);
    assert!(evaluate(Some(&open.state), &disabled, &ObserveThresholds::default()).is_err());

    let recovered = poll(73, rows, Some(&open));
    assert_eq!(output(&recovered)["transitions"], json!([]));
    assert_eq!(
        findings(&recovered, "lock-wait")[0]["since"],
        findings(&open, "lock-wait")[0]["since"]
    );
}

#[test]
fn same_epoch_backwards_clock_is_unknown() {
    let first = poll(1000, &[backend(1)], None);
    let earlier = snapshot(999, -100, &[backend(1)]);
    assert!(evaluate(Some(&first.state), &earlier, &ObserveThresholds::default()).is_err());
}

#[test]
fn lock_streaks_and_blocking_heads() {
    let mut a = backend(1);
    a["wait_event_type"] = "Lock".into();
    a["blocking_pids"] = json!([2, 2]);
    let mut b = backend(2);
    b["blocking_pids"] = json!([3]);
    let c = backend(3);
    let rows = &[c.clone(), a.clone(), b.clone()];
    let first = poll(10, rows, None);
    let second = poll(45, rows, Some(&first));
    let boundary = poll(70, rows, Some(&second));
    for r in [&first, &second, &boundary] {
        assert!(findings(r, "lock-wait").is_empty());
    }
    let two_polls = poll(71, rows, Some(&first));
    let many_polls = poll(71, rows, Some(&boundary));
    assert_eq!(
        findings(&two_polls, "lock-wait"),
        findings(&many_polls, "lock-wait")
    );
    let finding = &findings(&many_polls, "lock-wait")[0];
    assert_eq!(finding["blocking_heads"].as_array().unwrap().len(), 1);
    let head = &finding["blocking_heads"][0];
    assert_eq!(head["pid"], 3);
    assert_eq!(head["backend_start"], time(-20).replace("+00:00", "Z"));
    assert_eq!(head["query_id"], 42);
    assert_eq!(head["chain_pids"], json!([1, 2, 3]));
    b["blocking_pids"] = json!([1]);
    let cycle = poll(72, &[a.clone(), b], Some(&many_polls));
    assert_eq!(
        findings(&cycle, "lock-wait")[0]["blocking_heads"],
        json!([])
    );
    a["query_start"] = time(70).into();
    let changed = poll(150, &[a], Some(&cycle));
    assert!(findings(&changed, "lock-wait").is_empty());
    assert_eq!(output(&changed)["transitions"][0]["kind"], "resolved");
}

#[test]
fn deep_and_wide_lock_evidence_is_bounded() {
    let mut rows: Vec<_> = (1..=256)
        .map(|pid| {
            let mut row = backend(pid);
            if pid < 256 {
                row["wait_event_type"] = "Lock".into();
                row["blocking_pids"] = json!([pid + 1]);
            }
            row
        })
        .collect();
    let mut wide = backend(257);
    wide["wait_event_type"] = "Lock".into();
    wide["blocking_pids"] = json!((258..278).rev().collect::<Vec<_>>());
    rows.push(wide);
    let first = poll(0, &rows, None);
    let second = poll(61, &rows, Some(&first));
    let waits = findings(&second, "lock-wait");
    assert_eq!(waits.len(), 256);
    let longest = &waits[0]["blocking_heads"][0];
    assert_eq!(longest["pid"], 256);
    assert_eq!(longest["chain_len"], 256);
    assert_eq!(
        longest["chain_pids"],
        json!((193..=256).collect::<Vec<_>>())
    );
    let wide = waits.last().unwrap();
    assert_eq!(wide["blocking_head_count"], 20);
    assert_eq!(
        wide["blocking_heads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|head| head["pid"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        (258..274).collect::<Vec<_>>()
    );
    assert!(serde_json::to_vec(&second).unwrap().len() < 1024 * 1024);
}

#[test]
fn xmin_streaks_reset_when_advancing_or_cleared() {
    let mut row = backend(1);
    row["backend_xmin"] = "123".into();
    let first = poll(0, &[row.clone()], None);
    assert!(findings(&first, "horizon-pinned").is_empty());
    let mut previous = first.clone();
    for minutes in 1..=60 {
        previous = poll(minutes * 60, &[row.clone()], Some(&previous));
        assert!(findings(&previous, "horizon-pinned").is_empty());
    }
    for start in [&first, &previous] {
        assert_eq!(
            findings(&poll(3601, &[row.clone()], Some(start)), "horizon-pinned").len(),
            1
        );
    }
    row["backend_xmin"] = "124".into();
    assert!(findings(&poll(3601, &[row.clone()], Some(&first)), "horizon-pinned").is_empty());
    row["backend_xmin"] = Value::Null;
    let cleared = poll(3700, &[row.clone()], Some(&previous));
    row["backend_xmin"] = "123".into();
    assert!(findings(&poll(7400, &[row], Some(&cleared)), "horizon-pinned").is_empty());
}

#[test]
fn transaction_identity_and_restart_split_findings_and_streaks() {
    let mut row = backend(1);
    row["backend_xmin"] = "123".into();
    row["wait_event_type"] = "Lock".into();
    let first = poll(1000, &[row.clone()], None);
    let open = poll(4700, &[row.clone()], Some(&first));
    assert_eq!(
        findings(&open, "long-transaction")[0]["since"],
        findings(&first, "long-transaction")[0]["since"]
    );
    assert_ne!(
        findings(&open, "long-transaction")[0]["last_seen"],
        findings(&first, "long-transaction")[0]["last_seen"]
    );
    assert_eq!(output(&open)["open_findings"].as_array().unwrap().len(), 3);
    for field in ["backend_start", "xact_start"] {
        let mut changed = row.clone();
        changed[field] = time(10).into();
        let next = poll(8400, &[changed], Some(&open));
        assert!(findings(&next, "lock-wait").is_empty());
        assert!(findings(&next, "horizon-pinned").is_empty());
        assert_ne!(
            findings(&next, "long-transaction")[0]["since"],
            findings(&open, "long-transaction")[0]["since"]
        );
    }
    let restarted = observe(
        8400,
        4800,
        &[row],
        Some(&open),
        &ObserveThresholds::default(),
    );
    let report = output(&restarted);
    assert_eq!(report["open_findings"].as_array().unwrap().len(), 1);
    let transitions = report["transitions"].as_array().unwrap();
    assert_eq!(
        transitions.iter().filter(|t| t["kind"] == "ended").count(),
        3
    );
    assert!(transitions.iter().all(|t| t["kind"] != "resolved"));
    assert_ne!(
        report["open_findings"][0]["postmaster_start_time"],
        output(&open)["open_findings"][0]["postmaster_start_time"]
    );
}

#[test]
fn cli_uses_a_literal_database_and_accepts_previous_report_or_state() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    let root = tempfile::tempdir().unwrap();
    let worker = root.path().join("psql");
    let database = "host=example dbname=not-a-conninfo";
    fs::write(&worker, format!(r#"#!/bin/sh
set -eu
test "$*" = '-X -w -q -A -t -v ON_ERROR_STOP=1 -f -'
test "$PGHOST" = /run/postgresql
test "$PGPORT" = 5432
test "$PGDATABASE" = '{database}'
test "$PGUSER" = harbor_db_observer
test "$PGAPPNAME" = harbor-db-observer
test "$PGCONNECT_TIMEOUT" = 5
test "$PGOPTIONS" = '-c default_transaction_read_only=on -c statement_timeout=5000 -c lock_timeout=5000 -c search_path=pg_catalog'
test -z "${{PGSERVICE+x}}${{PGHOSTADDR+x}}${{PGPASSWORD+x}}"
sql=$(cat)
case "$sql" in *'BEGIN TRANSACTION READ ONLY;'*'COMMIT;') ;; *) exit 9 ;; esac
printf '%s\n' '{snapshot}'
"#, snapshot = json!({
        "observed_at": time(901), "postmaster_start_time": time(-100),
        "track_activities": true, "backends": [backend(1)]
    }))).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let cmd = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"));
        command
            .args(["observe", "--psql"])
            .arg(&worker)
            .args([
                "--database",
                database,
                "--label-budget",
                "label=900",
                "--label-budget",
                "other=42",
            ])
            .envs([
                ("PGHOST", "poison"),
                ("PGHOSTADDR", "poison"),
                ("PGSERVICE", "poison"),
                ("PGPASSWORD", "poison"),
                ("PGOPTIONS", "poison"),
            ]);
        command
    };
    let result = cmd().output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["transitions"][0]["kind"], "opened");
    for prior in [&report, &report["state"]] {
        let path = root.path().join("previous.json");
        fs::write(&path, prior.to_string()).unwrap();
        let result = cmd().arg("--previous").arg(&path).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let next: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(next["transitions"], json!([]));
        assert_eq!(next["open_findings"], report["open_findings"]);
    }
    // This prior fits on disk, but its ended finding expands when the CLI
    // escapes non-ASCII strings. Refuse it without writing a partial report.
    let mut prior = report["state"].clone();
    prior["postmaster_start_time"] = time(-101).into();
    prior["open_findings"][0]["hint"] = "é".repeat(6 * 1024 * 1024).into();
    let large = root.path().join("large-previous.json");
    fs::write(&large, serde_json::to_vec(&prior).unwrap()).unwrap();
    let result = cmd().arg("--previous").arg(&large).output().unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("report exceeded its size limit"));
    let broken = root.path().join("invalid.json");
    fs::write(&broken, "{}").unwrap();
    let result = cmd().arg("--previous").arg(&broken).output().unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    for args in [
        vec!["check"],
        vec!["observe", "--label-budget", "=1"],
        vec!["observe", "--label-budget", "label=0"],
        vec!["observe", "--process-timeout-seconds", "0"],
        vec!["observe", "--socket-dir", "/run/postgresql,127.0.0.1"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"))
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn cli_fails_closed_on_worker_error_invalid_snapshot_and_timeout() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::Command,
        time::{Duration, Instant},
    };

    let root = tempfile::tempdir().unwrap();
    let worker = root.path().join("psql");
    for script in [
        "cat >/dev/null; echo private-worker-diagnostic >&2; exit 1",
        "cat >/dev/null; echo '{\"query\":\"private-worker-diagnostic\"}'",
        "cat >/dev/null; sleep 60 & wait",
    ] {
        fs::write(&worker, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        let result = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"))
            .args(["observe", "--process-timeout-seconds", "1", "--psql"])
            .arg(&worker)
            .output()
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(!stderr.contains("private-worker-diagnostic"));
        if script.contains("sleep") {
            assert!(stderr.contains("execution limit"));
        }
    }
}
