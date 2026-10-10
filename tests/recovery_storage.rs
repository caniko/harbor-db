use harbor_db::storage::recovery;
use harbor_db::storage::{codec, durable, writer_fence};
use serde_json::Value;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};
// Each test still exercises real nonblocking contention. Serialize independent
// fixtures so concurrent fork/pre-exec windows cannot temporarily retain another
// test's just-closed descriptors before CLOEXEC runs.
static WORKERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn executable(path: &Path, script: &str) {
    fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
struct Fixture {
    root: tempfile::TempDir,
    config: Value,
    now: i64,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "data",
            "state",
            "restored",
            "package/bin",
            "backup/base/base-1",
            "backup/locks",
            "backup/evidence",
        ] {
            fs::create_dir_all(root.path().join(name)).unwrap();
        }
        for name in ["data", "backup/base/base-1"] {
            fs::write(root.path().join(name).join("PG_VERSION"), "18\n").unwrap();
            fs::write(root.path().join(name).join("identifier"), "12345").unwrap();
        }
        fs::write(
            root.path().join("data/postgresql.auto.conf"),
            "# original\n",
        )
        .unwrap();
        fs::write(root.path().join("backup/locks/mutate"), "").unwrap();
        fs::write(root.path().join("backup/LAST_SUCCESS"), "base-1\n").unwrap();
        fs::write(
            root.path().join("backup/base/base-1/backup_manifest"),
            "{\"WAL-Ranges\": []}\n",
        )
        .unwrap();
        durable::write_json(&root.path().join("backup/base/base-1.meta.json"),&json!({"backup_id":"base-1","system_identifier":"12345","pg_major":18,"epoch_id":"epoch-1","backup_stop_lsn":"0/100","post_backup_lsn":"0/200"})).unwrap();
        executable(
            &root.path().join("package/bin/pg_controldata"),
            "printf 'Database system identifier: '; cat \"$1/identifier\"; printf '\\n'",
        );
        executable(&root.path().join("package/bin/pg_ctl"), "exit 3");
        executable(
            &root.path().join("package/bin/pg_verifybackup"),
            &format!(
                "printf 'verify\\n' >> '{}'; test ! -e '{}'",
                root.path().join("verify.log").display(),
                root.path().join("corrupt").display()
            ),
        );
        let config = json!({"resource":"fixture","data_dir":root.path().join("data"),"state_dir":root.path().join("state"),"major":"18","package":root.path().join("package"),"recovery":{"system_identifier":"12345","backup_root":root.path().join("backup"),"snapshot_file":root.path().join("backup/evidence/records.json"),"receipt_file":root.path().join("backup/evidence/recovery.json"),"off_host_receipt_file":null,"source_hostname":"primary-host","max_age_seconds":3600,"verify_timeout_seconds":10,"record_checks":[{"name":"reviews","database":"app","sql":"SELECT records"}]}});
        fs::write(root.path().join("primary.json"),json!({"data_dir":config["data_dir"],"major":"18","system_identifier":"12345","fsync":"on","full_page_writes":"on","synchronous_commit":"on","in_recovery":false}).to_string()).unwrap();
        fs::write(root.path().join("restored.json"),json!({"data_dir":root.path().join("restored"),"major":"18","system_identifier":"12345","read_only":"on","in_recovery":false,"replay_lsn":"0/200"}).to_string()).unwrap();
        fs::write(root.path().join("rows"), "3:record-digest\r\n").unwrap();
        executable(
            &root.path().join("package/bin/psql"),
            &format!(
                "for arg in \"$@\"; do case \"$arg\" in *prepared_transactions*) cat '{}'; exit;; *default_transaction_read_only*) cat '{}'; exit;; *pg_control_system*) cat '{}'; exit;; esac; done; cat '{}'",
                root.path().join("fence.json").display(),
                root.path().join("restored.json").display(),
                root.path().join("primary.json").display(),
                root.path().join("rows").display()
            ),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Self { root, config, now }
    }
    fn p(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn snapshot(&self) -> Value {
        recovery::snapshot(
            &self.config,
            Path::new("/run/postgresql"),
            5432,
            Some(self.now),
        )
        .unwrap()
    }
    fn certify(&self) -> Value {
        recovery::certify(
            &self.config,
            &self.p("restored"),
            Path::new("/restore/socket"),
            55432,
            Some(self.now),
            Some("primary-host"),
        )
        .unwrap()
    }
    fn accepted(&self) {
        self.snapshot();
        self.certify();
    }
    fn receipt(&self) -> Value {
        durable::read_json(&self.p("backup/evidence/recovery.json")).unwrap()
    }
    fn source(&self) -> Value {
        durable::read_json(&self.p("backup/evidence/records.json")).unwrap()
    }
    fn fence(&mut self) -> String {
        self.config["recovery"]["require_writer_fence"] = json!(true);
        let opened = writer_fence::open_fence(&self.config, "12345").unwrap();
        let record = writer_fence::startup(&self.config).unwrap().unwrap();
        fs::write(self.p("fence.json"),json!({"data_dir":self.config["data_dir"],"major":"18","system_identifier":"12345","hba_file":record["hba_file"],"control_role":"postgres","in_recovery":false,"fsync":"on","full_page_writes":"on","synchronous_commit":"on","preload_libraries":[],"logical_subscriptions":0,"prepared_transactions":0,"other_writers":0}).to_string()).unwrap();
        opened["token"].as_str().unwrap().into()
    }
    fn preparation(&self) -> Value {
        for (name, script) in [
            (
                "readiness",
                format!(
                    "printf 'readiness\\n' >> '{}'",
                    self.p("preparation.log").display()
                ),
            ),
            (
                "backup-command",
                format!(
                    "printf 'backup\\n' >> '{}'",
                    self.p("preparation.log").display()
                ),
            ),
            (
                "restore-command",
                format!(
                    "printf 'restore\\n' >> '{}'; cp '{}' '{}'",
                    self.p("preparation.log").display(),
                    self.p("receipt-template.json").display(),
                    self.p("backup/evidence/recovery.json").display()
                ),
            ),
            (
                "export-command",
                format!(
                    "test -e '{}' || exit 8; printf 'export\\n' >> '{}'",
                    self.p("backup/evidence/recovery.json").display(),
                    self.p("preparation.log").display()
                ),
            ),
        ] {
            executable(&self.p(name), &script);
        }
        json!({"readiness_command":[self.p("readiness")],"backup_command":[self.p("backup-command")],"restore_command":[self.p("restore-command")]})
    }
    fn seed_template(&self) {
        self.accepted();
        fs::copy(
            self.p("backup/evidence/recovery.json"),
            self.p("receipt-template.json"),
        )
        .unwrap();
        fs::remove_file(self.p("backup/evidence/recovery.json")).unwrap();
        fs::remove_file(self.p("backup/evidence/records.json")).unwrap();
    }
    fn prepare(&self, p: &Value) -> harbor_db::storage::Result<Value> {
        recovery::prepare_at(
            &self.config,
            p,
            Path::new("/run/postgresql"),
            5432,
            Some(self.now),
        )
    }
}

fn python_recovery(f: &Fixture, action: &str, preparation: &Value) -> Value {
    use harbor_db::storage::process;
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import recovery
config,action,preparation,now=json.loads(sys.argv[1]),sys.argv[2],json.loads(sys.argv[3]),int(sys.argv[4])
try:
    if action == 'snapshot': result=recovery.snapshot(config,'/run/postgresql',5432,now=now)
    elif action == 'certify': result=recovery.certify(config,Path(config['data_dir']).parent/'restored','/restore/socket',55432,now=now,hostname='primary-host')
    elif action == 'check': result=recovery.check(config,now=now)
    elif action == 'live': result=recovery.live_check(config,'/run/postgresql',5432,now=now)
    elif action == 'prepare': result=recovery.prepare(config,preparation,'/run/postgresql',5432)
    else: raise AssertionError(action)
except Exception as error:
    result={'fixture_error':str(error)}
print(json.dumps(result))
"#;
    let mut command = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        f.config.to_string(),
        action.into(),
        preparation.to_string(),
        f.now.to_string(),
    ]);
    command.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    let result: Value = serde_json::from_slice(&process::execute(&command).unwrap()).unwrap();
    assert!(
        result.get("fixture_error").is_none(),
        "Python {action} rejected the synthetic fixture: {result}"
    );
    result
}

#[test]
fn python_and_rust_certify_each_others_fenced_snapshots_and_reuse_preparation_without_recapture() {
    let _workers = WORKERS.lock().unwrap();
    for python_first in [true, false] {
        let mut f = Fixture::new();
        let token = f.fence();
        let source = if python_first {
            python_recovery(&f, "snapshot", &json!({}))
        } else {
            f.snapshot()
        };
        let snapshot_bytes = fs::read(f.p("backup/evidence/records.json")).unwrap();
        assert_eq!(source["writer_fence_token"], token);
        let certified = if python_first {
            f.certify()
        } else {
            python_recovery(&f, "certify", &json!({}))
        };
        let receipt_bytes = fs::read(f.p("backup/evidence/recovery.json")).unwrap();
        assert_eq!(certified["snapshot_sha256"], codec::digest(&snapshot_bytes));
        assert_eq!(
            python_recovery(&f, "check", &json!({})),
            recovery::check(&f.config, Some(f.now)).unwrap()
        );
        assert_eq!(
            python_recovery(&f, "live", &json!({})),
            recovery::live_check(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now))
                .unwrap()
        );
        assert_eq!(
            fs::read(f.p("backup/evidence/recovery.json")).unwrap(),
            receipt_bytes
        );
        let reproduced_receipt = if python_first {
            python_recovery(&f, "certify", &json!({}))
        } else {
            f.certify()
        };
        assert_eq!(reproduced_receipt, certified);
        assert_eq!(
            fs::read(f.p("backup/evidence/recovery.json")).unwrap(),
            receipt_bytes
        );
        // Independent producer readback uses the same retained timestamp so any
        // JSON or universal-newline difference changes the receipt binding.
        let reproduced = if python_first {
            f.snapshot()
        } else {
            python_recovery(&f, "snapshot", &json!({}))
        };
        assert_eq!(reproduced, source);
        assert_eq!(
            fs::read(f.p("backup/evidence/records.json")).unwrap(),
            snapshot_bytes
        );
        let preparation = f.preparation();
        for command in ["backup-command", "restore-command"] {
            executable(
                &f.p(command),
                &format!("touch '{}'; exit 9", f.p("unexpected-recapture").display()),
            );
        }
        assert_eq!(f.prepare(&preparation).unwrap()["status"], "ready");
        assert_eq!(
            python_recovery(&f, "prepare", &preparation)["status"],
            "ready"
        );
        assert!(!f.p("unexpected-recapture").exists());
        assert_eq!(
            fs::read(f.p("backup/evidence/records.json")).unwrap(),
            snapshot_bytes
        );
        assert_eq!(
            fs::read(f.p("backup/evidence/recovery.json")).unwrap(),
            receipt_bytes
        );
        assert_eq!(
            writer_fence::startup(&f.config).unwrap().unwrap()["token"],
            token
        );
        assert!(
            !f.p("state/identity.json").exists(),
            "readiness must not adopt storage"
        );
    }
}

#[test]
fn timestamp_and_lsn_parity_are_strict() {
    assert_eq!(
        recovery::lsn(&json!("FFFFFFFF/FFFFFFFF")).unwrap(),
        u64::MAX
    );
    for value in [
        json!("1/100000000"),
        json!("-1/0"),
        json!(null),
        json!("1/2\n"),
    ] {
        assert!(recovery::lsn(&value).is_err());
    }
    for timestamp in [
        json!(true),
        json!(100.0),
        json!(101),
        json!(89),
        json!(null),
    ] {
        assert!(recovery::fresh(&timestamp, 100, 10).is_err());
    }
    assert!(recovery::fresh(&json!(90), 100, 10).is_ok());
}

#[test]
fn contract_hash_matches_legacy_sorted_ascii_json() {
    let settings =
        json!({"record_checks":[{"sql":"SELECT 'é'","database":"postgres","name":"rows"}]});
    let legacy =
        b"[{\"database\": \"postgres\", \"name\": \"rows\", \"sql\": \"SELECT '\\u00e9'\"}]";
    assert_eq!(
        recovery::contract(&settings).unwrap(),
        harbor_db::storage::codec::digest(legacy)
    );
}

#[test]
fn policy_rejects_overwrites_boolean_ages_and_duplicate_records() {
    let settings = json!({"system_identifier":"12345","snapshot_file":"/a","receipt_file":"/b","max_age_seconds":10,"record_checks":[{"name":"rows","database":"postgres","sql":"SELECT 1"}]});
    assert!(recovery::policy(&json!({"recovery":settings})).is_ok());
    for (key, value) in [
        ("receipt_file", json!("/a")),
        ("max_age_seconds", json!(true)),
        ("system_identifier", json!("0123")),
        ("require_writer_fence", json!(1)),
        ("record_checks", json!([])),
    ] {
        let mut bad = settings.clone();
        bad[key] = value;
        assert!(recovery::policy(&json!({"recovery":bad})).is_err());
    }
}

#[test]
fn matching_records_certify_exact_backup_and_snapshot_bytes() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let source = f.snapshot();
    assert_eq!(
        source["records"]["reviews"],
        codec::digest(b"3:record-digest\n")
    );
    let receipt = f.certify();
    assert_eq!(receipt["status"], "ready");
    assert_eq!(
        receipt["snapshot_sha256"],
        codec::file_digest(&f.p("backup/evidence/records.json")).unwrap()
    );
    assert_eq!(
        receipt["manifest_sha256"],
        codec::file_digest(&f.p("backup/base/base-1/backup_manifest")).unwrap()
    );
    assert_eq!(
        recovery::check(&f.config, Some(f.now)).unwrap()["backup_id"],
        "base-1"
    );
    assert_eq!(
        recovery::live_check(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).unwrap()["snapshot_sha256"],
        receipt["snapshot_sha256"]
    );
    assert!(!f.p("state/identity.json").exists());
}

#[test]
fn fractional_verification_timeout_runs_backup_verification() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.config["recovery"]["verify_timeout_seconds"] = json!(0.5);
    let source = f.snapshot();
    assert_eq!(source["backup_id"], "base-1");
    assert_eq!(fs::read_to_string(f.p("verify.log")).unwrap(), "verify\n");
}

#[test]
fn verification_timeout_retains_source_for_explicit_retry() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.config["recovery"]["verify_timeout_seconds"] = json!(0.05);
    executable(&f.p("package/bin/pg_verifybackup"), "sleep 1");
    assert!(
        recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
    );
    assert!(!f.p("backup/evidence/records.json").exists());
    assert!(f.p("backup/base/base-1/backup_manifest").exists());
    executable(&f.p("package/bin/pg_verifybackup"), "exit 0");
    assert_eq!(f.snapshot()["backup_id"], "base-1");
}

#[test]
fn managed_preparation_retains_local_evidence_across_missing_off_host_retry() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.fence();
    f.accepted();
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    let original_source = fs::read(f.p("backup/evidence/records.json")).unwrap();
    let original_receipt = fs::read(f.p("backup/evidence/recovery.json")).unwrap();
    let readiness = f.p("readiness");
    executable(&readiness, "exit 0");
    let forbidden = f.p("forbidden");
    executable(
        &forbidden,
        &format!("touch '{}'; exit 1", f.p("replaced").display()),
    );
    let export = f.p("export");
    executable(
        &export,
        &format!("printf 'export\n' >> '{}'", f.p("exports").display()),
    );
    let preparation = json!({"readiness_command":[readiness],"backup_command":[forbidden],"restore_command":[forbidden],"export_command":[export]});
    for _ in 0..2 {
        assert!(
            recovery::prepare_at(
                &f.config,
                &preparation,
                Path::new("/run/postgresql"),
                5432,
                Some(f.now)
            )
            .is_err()
        );
        assert_eq!(
            fs::read(f.p("backup/evidence/records.json")).unwrap(),
            original_source
        );
        assert_eq!(
            fs::read(f.p("backup/evidence/recovery.json")).unwrap(),
            original_receipt
        );
        assert!(!f.p("replaced").exists());
        assert!(writer_fence::startup(&f.config).unwrap().is_some());
    }
    assert_eq!(
        fs::read_to_string(f.p("exports")).unwrap(),
        "export\nexport\n"
    );
    let mut remote = f.receipt();
    remote["executor_host"] = json!("independent-host");
    durable::write_json(&f.p("backup/evidence/off-host.json"), &remote).unwrap();
    assert_eq!(
        recovery::prepare_at(
            &f.config,
            &preparation,
            Path::new("/run/postgresql"),
            5432,
            Some(f.now)
        )
        .unwrap()["status"],
        "ready"
    );
}

#[test]
fn missing_evidence_and_missing_lease_anchors_are_never_created_by_admission() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    assert!(!f.p("backup/evidence/recovery.lock").exists());
    f.snapshot();
    assert!(recovery::preflight(&f.config, Some(f.now)).is_err());
    f.certify();
    fs::remove_file(f.p("backup/evidence/recovery.lock")).unwrap();
    assert!(recovery::preflight(&f.config, Some(f.now)).is_err());
    assert!(!f.p("backup/evidence/recovery.lock").exists());
    fs::remove_file(f.p("backup/locks/mutate")).unwrap();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    assert!(!f.p("backup/locks/mutate").exists());
}

#[test]
fn preflight_defers_byte_verification_but_requires_fresh_bound_receipts() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    let before = fs::read(f.p("verify.log")).unwrap();
    fs::write(f.p("corrupt"), "").unwrap();
    assert_eq!(
        recovery::preflight(&f.config, Some(f.now)).unwrap()["status"],
        "preflight-ready"
    );
    assert_eq!(fs::read(f.p("verify.log")).unwrap(), before);
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    for now in [f.now - 1, f.now + 3601] {
        assert!(recovery::preflight(&f.config, Some(now)).is_err());
    }
    let mut bad = f.config.clone();
    bad["recovery"]["record_checks"][0]["sql"] = json!("SELECT different_records");
    assert!(recovery::preflight(&bad, Some(f.now)).is_err());
}

#[test]
fn loss_and_changed_source_records_cannot_publish_or_reuse_certification() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.snapshot();
    fs::write(f.p("rows"), "2:missing-review\n").unwrap();
    assert!(
        recovery::certify(
            &f.config,
            &f.p("restored"),
            Path::new("/restore/socket"),
            55432,
            Some(f.now),
            Some("primary-host")
        )
        .is_err()
    );
    assert!(!f.p("backup/evidence/recovery.json").exists());
    fs::write(f.p("rows"), "3:record-digest\n").unwrap();
    f.certify();
    fs::write(f.p("rows"), "3:changed-review\n").unwrap();
    assert!(
        recovery::live_check(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
    );
    f.snapshot();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
}

#[test]
fn backup_manifest_metadata_identity_and_completed_identifier_changes_invalidate_evidence() {
    let _workers = WORKERS.lock().unwrap();
    for target in [
        "backup/base/base-1/backup_manifest",
        "backup/base/base-1.meta.json",
    ] {
        let f = Fixture::new();
        f.accepted();
        let mut bytes = fs::read(f.p(target)).unwrap();
        bytes.push(b' ');
        fs::write(f.p(target), bytes).unwrap();
        assert!(recovery::check(&f.config, Some(f.now)).is_err(), "{target}");
    }
    for id in ["../data", "base-1.partial", "", "base/1"] {
        let f = Fixture::new();
        fs::write(f.p("backup/LAST_SUCCESS"), id).unwrap();
        assert!(
            recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err(),
            "{id}"
        );
    }
    let f = Fixture::new();
    f.accepted();
    fs::write(f.p("backup/base/base-1/identifier"), "99999").unwrap();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    let f = Fixture::new();
    let meta = f.p("backup/base/base-1.meta.json");
    let mut value = durable::read_json(&meta).unwrap();
    value["post_backup_lsn"] = json!("0/100");
    durable::write_json(&meta, &value).unwrap();
    assert!(
        recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
    );
}

#[test]
fn disposable_restore_must_be_distinct_read_only_primary_at_recovery_point() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.snapshot();
    let original = durable::read_json(&f.p("restored.json")).unwrap();
    for (key, bad) in [
        ("data_dir", f.config["data_dir"].clone()),
        ("major", json!("17")),
        ("read_only", json!("off")),
        ("in_recovery", json!(true)),
        ("replay_lsn", json!("0/100")),
        ("system_identifier", json!("99999")),
        ("replay_lsn", json!(null)),
    ] {
        let mut observation = original.clone();
        observation[key] = bad;
        fs::write(f.p("restored.json"), observation.to_string()).unwrap();
        assert!(
            recovery::certify(
                &f.config,
                &f.p("restored"),
                Path::new("/restore/socket"),
                55432,
                Some(f.now),
                Some("host")
            )
            .is_err(),
            "{key}"
        );
        assert!(!f.p("backup/evidence/recovery.json").exists());
    }
    assert!(
        recovery::certify(
            &f.config,
            &f.p("data"),
            Path::new("/restore/socket"),
            55432,
            Some(f.now),
            Some("host")
        )
        .is_err()
    );
}

#[test]
fn hostile_receipts_cannot_claim_record_level_acceptance() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    let good = f.receipt();
    for (key, bad) in [
        ("status", json!("claimed")),
        ("backup_id", json!("other")),
        ("records", json!({"reviews":"0".repeat(64)})),
        ("snapshot_sha256", json!("0".repeat(64))),
        ("restored_data_dir", f.config["data_dir"].clone()),
        ("restored_data_dir", json!("")),
        ("restored_data_dir", json!(true)),
        ("completed_at", json!(f.now - 1)),
        ("completed_at", json!(true)),
        ("completed_at", json!(f.now + 1)),
        ("replay_lsn", json!("0/100")),
        ("replay_lsn", json!("hostile")),
        ("version", json!(2)),
        ("record_contract_sha256", json!("changed")),
    ] {
        let mut bad_receipt = good.clone();
        bad_receipt[key] = bad;
        durable::write_json(&f.p("backup/evidence/recovery.json"), &bad_receipt).unwrap();
        assert!(recovery::check(&f.config, Some(f.now)).is_err(), "{key}");
    }
    durable::write_json(&f.p("backup/evidence/recovery.json"), &good).unwrap();
    recovery::check(&f.config, Some(f.now)).unwrap();
}

#[test]
fn incomplete_or_malformed_snapshot_records_never_admit() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    let source = f.source();
    for records in [
        json!({}),
        json!({"reviews":"A".repeat(64)}),
        json!({"reviews":"0".repeat(63)}),
        json!({"reviews":17}),
        json!({"reviews":"0".repeat(64),"extra":"0".repeat(64)}),
        json!(null),
    ] {
        let mut bad = source.clone();
        bad["records"] = records;
        durable::write_json(&f.p("backup/evidence/records.json"), &bad).unwrap();
        assert!(recovery::preflight(&f.config, Some(f.now)).is_err());
    }
}

#[test]
fn off_host_import_requires_independent_executor_and_exact_source_binding() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.accepted();
    let good = f.receipt();
    let remote = f.p("backup/evidence/off-host.json");
    f.config["recovery"]["off_host_receipt_file"] = json!(remote);
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    let incoming = f.p("incoming.json");
    for changes in [
        json!({}),
        json!({"executor_host":"remote","backup_id":"other"}),
        json!({"executor_host":"remote","records":{"reviews":"0".repeat(64)}}),
        json!({"executor_host":""}),
        json!({"executor_host":true}),
    ] {
        let mut receipt = good.clone();
        for (k, v) in changes.as_object().unwrap() {
            receipt[k] = v.clone();
        }
        durable::write_json(&incoming, &receipt).unwrap();
        assert!(recovery::import_off_host(&f.config, &incoming, Some(f.now)).is_err());
        assert!(!remote.exists());
    }
    let mut receipt = good.clone();
    receipt["executor_host"] = json!("independent-host");
    durable::write_json(&incoming, &receipt).unwrap();
    recovery::import_off_host(&f.config, &incoming, Some(f.now)).unwrap();
    assert_eq!(
        recovery::check(&f.config, Some(f.now)).unwrap()["off_host"],
        "independent-host"
    );
    assert_eq!(durable::read_json(&remote).unwrap(), receipt);
}

#[test]
fn redirected_snapshot_receipt_and_backup_ancestors_are_rejected() {
    let _workers = WORKERS.lock().unwrap();
    for name in [
        "backup/evidence/records.json",
        "backup/evidence/recovery.json",
        "backup/base/base-1/backup_manifest",
    ] {
        let f = Fixture::new();
        f.accepted();
        let selected = f.p(name);
        let saved = selected.with_extension("saved");
        fs::rename(&selected, &saved).unwrap();
        symlink(&saved, &selected).unwrap();
        assert!(recovery::check(&f.config, Some(f.now)).is_err(), "{name}");
    }
    let f = Fixture::new();
    f.accepted();
    fs::rename(f.p("backup/evidence"), f.p("evidence-saved")).unwrap();
    symlink(f.p("evidence-saved"), f.p("backup/evidence")).unwrap();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
}

#[test]
fn backup_and_evidence_leases_exclude_mutation_through_authority_publication() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    for anchor in ["backup/locks/mutate", "backup/evidence/recovery.lock"] {
        let lease = durable::lock(&f.p(anchor), false, false).unwrap();
        assert!(recovery::check(&f.config, Some(f.now)).is_err());
        assert!(
            recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
        );
        drop(lease);
    }
    let accepted = recovery::admission_lease(&f.config, Some(f.now), true, None, 5432).unwrap();
    for anchor in ["backup/locks/mutate", "backup/evidence/recovery.lock"] {
        assert!(durable::lock(&f.p(anchor), false, false).is_err());
    }
    drop(accepted);
    for anchor in ["backup/locks/mutate", "backup/evidence/recovery.lock"] {
        durable::lock(&f.p(anchor), false, false).unwrap();
    }
}

#[test]
fn required_fence_binds_snapshots_certifiers_and_retained_authority_window() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.config["recovery"]["require_writer_fence"] = json!(true);
    assert!(
        recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
    );
    assert!(!f.p("backup/evidence/records.json").exists());
    let token = f.fence();
    let source = f.snapshot();
    assert_eq!(source["writer_fence_token"], token);
    // Disposable certification uses copied evidence, independent of active primary journal.
    let marker = f.p("state/writer-fence.json");
    fs::rename(&marker, f.p("held.json")).unwrap();
    f.certify();
    fs::rename(f.p("held.json"), &marker).unwrap();
    let accepted = recovery::admission_lease(&f.config, Some(f.now), true, None, 5432).unwrap();
    assert!(writer_fence::close_fence(&f.config, &token).is_err());
    drop(accepted);
    writer_fence::close_fence(&f.config, &token).unwrap();
    f.fence();
    assert!(recovery::check(&f.config, Some(f.now)).is_err());
    f.config["recovery"]["require_writer_fence"] = json!(false);
    recovery::check(&f.config, Some(f.now)).unwrap();
}

#[test]
fn disposable_certifier_rejects_missing_or_invalid_copied_fence_tokens() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.fence();
    let source = f.snapshot();
    for token in [
        json!(null),
        json!("other-window"),
        json!("a".repeat(31)),
        json!(17),
    ] {
        let mut bad = source.clone();
        bad["writer_fence_token"] = token;
        durable::write_json(&f.p("backup/evidence/records.json"), &bad).unwrap();
        assert!(
            recovery::certify(
                &f.config,
                &f.p("restored"),
                Path::new("/restore/socket"),
                55432,
                Some(f.now),
                Some("host")
            )
            .is_err()
        );
        assert!(!f.p("backup/evidence/recovery.json").exists());
    }
}

#[test]
fn required_fence_live_inspection_rejects_other_writers_before_capture() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.fence();
    let mut observation = durable::read_json(&f.p("fence.json")).unwrap();
    observation["other_writers"] = json!(1);
    fs::write(f.p("fence.json"), observation.to_string()).unwrap();
    assert!(
        recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
    );
    assert!(!f.p("backup/evidence/records.json").exists());
}

#[test]
fn queries_are_local_read_only_normalized_and_workers_retain_explicit_leases() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let anchor = f.p("backup/locks/mutate");
    let lease = durable::lock(&anchor, true, false).unwrap();
    executable(
        &f.p("package/bin/psql"),
        &format!(
            "test -e /proc/self/fd/{} || exit 7\ncase \"$*\" in *'BEGIN READ ONLY;'*'SET LOCAL TimeZone'*'SET LOCAL DateStyle'*'SET LOCAL bytea_output'*) ;; *) exit 9;; esac\nflock -n -x '{}' -c true && exit 8\nprintf 'rows\\r\\n'",
            lease.fd(),
            anchor.display()
        ),
    );
    assert_eq!(
        recovery::query_leased(
            &f.config,
            Path::new("/local/socket"),
            5432,
            "app",
            "SELECT 1",
            &[lease.fd()]
        )
        .unwrap(),
        "rows\n"
    );
    assert!(recovery::query(&f.config, Path::new("localhost"), 5432, "app", "SELECT 1").is_err());
    assert!(
        recovery::query(
            &f.config,
            Path::new("/local,socket"),
            5432,
            "app",
            "SELECT 1"
        )
        .is_err()
    );
    assert!(recovery::query(&f.config, Path::new("/local/socket"), 0, "app", "SELECT 1").is_err());
}

#[test]
fn preparation_captures_first_snapshot_and_reuses_exact_evidence_on_retry() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.seed_template();
    let prep = f.preparation();
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\nrestore\n"
    );
    let source = fs::read(f.p("backup/evidence/records.json")).unwrap();
    let receipt = fs::read(f.p("backup/evidence/recovery.json")).unwrap();
    assert!(f.p("backup/evidence/preparation.json").exists());
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\nrestore\nreadiness\n"
    );
    assert_eq!(
        fs::read(f.p("backup/evidence/records.json")).unwrap(),
        source
    );
    assert_eq!(
        fs::read(f.p("backup/evidence/recovery.json")).unwrap(),
        receipt
    );
    assert_eq!(
        fs::read_to_string(f.p("backup/LAST_SUCCESS")).unwrap(),
        "base-1\n"
    );
}

#[test]
fn preparation_readiness_failure_never_captures_backup_or_snapshot() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let prep = f.preparation();
    executable(
        &f.p("readiness"),
        &format!(
            "printf 'readiness\\n' >> '{}'; exit 1",
            f.p("preparation.log").display()
        ),
    );
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\n"
    );
    assert!(!f.p("backup/evidence/records.json").exists());
    assert!(!f.p("backup/evidence/preparation.json").exists());
}

#[test]
fn preparation_rejects_stale_snapshot_and_stale_backup_without_replacement() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    let prep = f.preparation();
    let source = fs::read(f.p("backup/evidence/records.json")).unwrap();
    assert!(
        recovery::prepare_at(
            &f.config,
            &prep,
            Path::new("/run/postgresql"),
            5432,
            Some(f.now + 3601)
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\n"
    );
    assert_eq!(
        fs::read(f.p("backup/evidence/records.json")).unwrap(),
        source
    );
    let f = Fixture::new();
    let prep = f.preparation();
    assert!(
        recovery::prepare_at(
            &f.config,
            &prep,
            Path::new("/run/postgresql"),
            5432,
            Some(f.now + 3601)
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\n"
    );
    assert!(!f.p("backup/evidence/preparation.json").exists());
    assert!(!f.p("backup/evidence/records.json").exists());
}

#[test]
fn interrupted_capture_resumes_retained_backup_journal_without_recapture() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.seed_template();
    let prep = f.preparation();
    fs::write(f.p("corrupt"), "").unwrap();
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\n"
    );
    let journal = fs::read(f.p("backup/evidence/preparation.json")).unwrap();
    assert!(!f.p("backup/evidence/records.json").exists());
    fs::remove_file(f.p("corrupt")).unwrap();
    assert_eq!(f.prepare(&prep).unwrap()["status"], "ready");
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\nreadiness\nrestore\n"
    );
    assert_eq!(
        fs::read(f.p("backup/evidence/preparation.json")).unwrap(),
        journal
    );
}

#[test]
fn interrupted_capture_refuses_replacement_backup_and_orphan_receipts() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.seed_template();
    let prep = f.preparation();
    fs::write(f.p("corrupt"), "").unwrap();
    assert!(f.prepare(&prep).is_err());
    fs::remove_file(f.p("corrupt")).unwrap();
    let meta = f.p("backup/base/base-1.meta.json");
    let mut value = durable::read_json(&meta).unwrap();
    value["epoch_id"] = json!("replacement");
    durable::write_json(&meta, &value).unwrap();
    assert!(
        f.prepare(&prep)
            .unwrap_err()
            .to_string()
            .contains("backup changed")
    );
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\nreadiness\n"
    );
    assert!(!f.p("backup/evidence/records.json").exists());
    let f = Fixture::new();
    let prep = f.preparation();
    fs::write(f.p("backup/evidence/recovery.json"), "{}").unwrap();
    assert!(
        f.prepare(&prep)
            .unwrap_err()
            .to_string()
            .contains("without their bound")
    );
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\n"
    );
}

#[test]
fn preparation_export_runs_after_local_acceptance_before_off_host_abort() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.accepted();
    let mut prep = f.preparation();
    prep["export_command"] = json!([f.p("export-command")]);
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nexport\n"
    );
    let mut receipt = f.receipt();
    receipt["executor_host"] = json!("independent-host");
    durable::write_json(&f.p("backup/evidence/off-host.json"), &receipt).unwrap();
    assert_eq!(f.prepare(&prep).unwrap()["off_host"], "independent-host");
}

#[test]
fn preparation_validates_all_argv_and_required_fence_before_commands() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    let prep = f.preparation();
    for key in [
        "readiness_command",
        "backup_command",
        "restore_command",
        "export_command",
    ] {
        let mut bad = prep.clone();
        bad[key] = json!(["relative"]);
        assert!(f.prepare(&bad).is_err());
        assert!(!f.p("preparation.log").exists());
    }
    f.config["recovery"]["require_writer_fence"] = json!(true);
    assert!(f.prepare(&prep).is_err());
    assert!(!f.p("preparation.log").exists());
}

#[test]
fn preparation_nested_workers_inherit_outer_preparation_anchor() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.seed_template();
    let prep = f.preparation();
    let anchor = f.p("backup/evidence/preparation.lock");
    // Executed by the nested snapshot's physical, verification, and SQL workers.
    // Looking up /proc/self/fd tests actual inheritance, not just parent locking.
    let check = format!(
        "found=no; for fd in /proc/self/fd/*; do test \"$(readlink \"$fd\")\" = '{}' && found=yes; done; test \"$found\" = yes || exit 19",
        anchor.display()
    );
    for name in ["pg_controldata", "pg_verifybackup", "psql"] {
        let target = f.p(&format!("package/bin/{name}"));
        let script = fs::read_to_string(&target).unwrap();
        executable(
            &target,
            &format!("{check}\n{}", script.strip_prefix("#!/bin/sh\n").unwrap()),
        );
    }
    f.prepare(&prep).unwrap();
}

// A child test process gives preparation its own credential environment and
// lifetime, without mutating this multithreaded test process's environment.
#[test]
fn preparation_child_driver() {
    let Some(root) = std::env::var_os("HARBOR_DB_PREPARATION_TEST_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let config = durable::read_json(&root.join("driver-config.json")).unwrap();
    let preparation = durable::read_json(&root.join("driver-preparation.json")).unwrap();
    let now = fs::read_to_string(root.join("driver-now"))
        .unwrap()
        .parse()
        .unwrap();
    let result = recovery::prepare_at(
        &config,
        &preparation,
        Path::new("/run/postgresql"),
        5432,
        Some(now),
    );
    durable::write_json(
        &root.join("driver-result.json"),
        &match result {
            Ok(value) => json!({"ok":true,"value":value}),
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        },
    )
    .unwrap();
}

fn preparation_driver(f: &Fixture, preparation: &Value) -> std::process::Child {
    durable::write_json(&f.p("driver-config.json"), &f.config).unwrap();
    durable::write_json(&f.p("driver-preparation.json"), preparation).unwrap();
    fs::write(f.p("driver-now"), f.now.to_string()).unwrap();
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "preparation_child_driver"])
        .env("HARBOR_DB_PREPARATION_TEST_CHILD", f.root.path())
        .env("CREDENTIALS_DIRECTORY", f.p("credentials"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn managed_credentials_import_rejects_local_and_unbound_receipts_then_accepts_independent_execution()
 {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.accepted();
    let prep = f.preparation();
    let receipt = f.receipt();
    fs::create_dir(f.p("credentials")).unwrap();
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    for changes in [
        json!({}),
        json!({"executor_host":"remote","backup_id":"other"}),
        json!({"executor_host":"remote","records":{"reviews":"0".repeat(64)}}),
    ] {
        let mut bad = receipt.clone();
        for (key, value) in changes.as_object().unwrap() {
            bad[key] = value.clone();
        }
        durable::write_json(&f.p("credentials/recovery-off-host"), &bad).unwrap();
        assert!(preparation_driver(&f, &prep).wait().unwrap().success());
        assert_eq!(
            durable::read_json(&f.p("driver-result.json")).unwrap()["ok"],
            false
        );
        assert!(!f.p("backup/evidence/off-host.json").exists());
    }
    let mut independent = receipt;
    independent["executor_host"] = json!("independent-host");
    durable::write_json(&f.p("credentials/recovery-off-host"), &independent).unwrap();
    assert!(preparation_driver(&f, &prep).wait().unwrap().success());
    let result = durable::read_json(&f.p("driver-result.json")).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["value"]["off_host"], "independent-host");
}

#[test]
fn nested_snapshot_worker_retains_preparation_lease_after_coordinator_death() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let prep = f.preparation();
    let target = f.p("package/bin/psql");
    let original = fs::read_to_string(&target).unwrap();
    let ready = f.p("nested-worker-ready");
    let release = f.p("nested-worker-release");
    executable(
        &target,
        &format!(
            "case \"$*\" in *pg_control_system*) ;; *) printf '%s' \"$$\" > '{}'; n=0; while test ! -e '{}' && test \"$n\" -lt 200; do sleep 0.05; n=$((n + 1)); done;; esac\n{}",
            ready.display(),
            release.display(),
            original.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    );
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"release");
        }
    }
    let _release = Release(release.clone());
    let mut coordinator = preparation_driver(&f, &prep);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "nested worker did not begin"
        );
        assert!(
            coordinator.try_wait().unwrap().is_none(),
            "coordinator exited before nested capture"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    coordinator.kill().unwrap();
    coordinator.wait().unwrap();
    let anchor = f.p("backup/evidence/preparation.lock");
    assert!(
        durable::lock(&anchor, false, false).is_err(),
        "coordinator death released preparation while nested worker was alive"
    );
    fs::write(release, b"release").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(lease) = durable::lock(&anchor, false, false) {
            drop(lease);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested worker did not release lease after exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(f.p("backup/evidence/preparation.json").exists());
    assert!(!f.p("backup/evidence/records.json").exists());
}
