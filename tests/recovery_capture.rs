//! Producer admission tests; no synthetic fixture qualifies physical recovery.
use harbor_db::storage::{process, recovery_capture};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn cli_legacy_capture_rejects_without_mutating_selected_evidence() {
    let root = tempfile::tempdir().unwrap();
    let local = root.path().join("backup/recovery");
    for name in ["captures", "snapshots", "pins"] {
        fs::create_dir_all(local.join(name)).unwrap();
    }
    let paths = [
        "SELECTED",
        "captures/old.json",
        "snapshots/old.json",
        "pins/old.json",
    ]
    .map(|name| local.join(name));
    for (index, path) in paths.iter().enumerate() {
        fs::write(path, format!("previous evidence {index}\n")).unwrap();
    }
    let previous = paths
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path).unwrap();
            (metadata.dev(), metadata.ino(), fs::read(path).unwrap())
        })
        .collect::<Vec<_>>();
    let config = root.path().join("config.json");
    fs::write(&config, json!({"recovery":{"repository_protocol":"legacy",
        "backup_root":root.path().join("backup"), "snapshot_file":local.join("snapshots/old.json")}}).to_string()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"));
    command
        .arg("--config")
        .arg(&config)
        .args([
            "capture-backup",
            "--backup-id",
            "base-1",
            "--capture-id",
            "new",
            "--socket-dir",
            "/socket",
            "--port",
            "5432",
            "--wal-wait-seconds",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = process::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "harbor-db-postgres: capture requires source-local-v1\n"
    );
    for (path, before) in paths.iter().zip(previous) {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino(), fs::read(path).unwrap()),
            before
        );
    }
    assert_eq!(fs::read_dir(&local).unwrap().count(), 4);
    for name in ["captures", "snapshots", "pins"] {
        assert_eq!(fs::read_dir(local.join(name)).unwrap().count(), 1);
    }
}

#[test]
fn legacy_capture_is_rejected_before_any_publication() {
    let error = recovery_capture::finalize(
        &json!({"recovery":{"repository_protocol":"legacy"}}),
        "base-1",
        "capture-1",
        Path::new("/socket"),
        5432,
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(error.to_string().contains("source-local-v1"));
}

#[test]
fn source_local_capture_requires_explicit_writer_fence() {
    let error = recovery_capture::finalize(
        &json!({"recovery":{"repository_protocol":"source-local-v1"}}),
        "base-1",
        "capture-1",
        Path::new("/socket"),
        5432,
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(error.to_string().contains("writer fence"));
}

#[test]
fn unsafe_identifiers_are_rejected_before_opening_repository() {
    let config =
        json!({"recovery":{"repository_protocol":"source-local-v1", "require_writer_fence":true}});
    for id in ["../escape", "bad.partial", "", "/absolute"] {
        assert!(
            recovery_capture::finalize(
                &config,
                id,
                "capture-1",
                Path::new("/socket"),
                5432,
                Duration::ZERO
            )
            .unwrap_err()
            .to_string()
            .contains("identifier")
        );
        assert!(
            recovery_capture::finalize(
                &config,
                "base-1",
                id,
                Path::new("/socket"),
                5432,
                Duration::ZERO
            )
            .unwrap_err()
            .to_string()
            .contains("identifier")
        );
    }
}
