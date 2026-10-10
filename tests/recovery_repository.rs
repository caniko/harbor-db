//! Reader fixtures only: these binaries do not qualify a physical WAL producer.
use harbor_db::storage::{codec, durable, process, recovery, recovery_repository, writer_fence};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

struct Fixture {
    root: tempfile::TempDir,
    config: Value,
    now: i64,
    meta: Value,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "backup/base/base-1",
            "backup/recovery/captures",
            "backup/recovery/pins",
            "backup/recovery/snapshots",
            "package/bin",
            "backup/evidence",
            "backup/locks",
            "data",
            "restored",
            "state",
        ] {
            fs::create_dir_all(root.path().join(name)).unwrap();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let config = json!({"major":"18", "package":root.path().join("package"), "recovery":{
            "backup_root":root.path().join("backup"), "system_identifier":"12345", "max_age_seconds":3600,
            "snapshot_file":root.path().join("backup/evidence/snapshot.json"), "receipt_file":root.path().join("backup/evidence/receipt.json"),
            "record_checks":[{"name":"rows", "database":"app", "sql":"SELECT records"}]}});
        let manifest = root.path().join("backup/base/base-1/backup_manifest");
        fs::write(&manifest, "{\"WAL-Ranges\": [{\"Timeline\": 1, \"Start-LSN\": \"0/80\", \"End-LSN\": \"0/100\"}]}\n").unwrap();
        fs::write(root.path().join("backup/base/base-1/PG_VERSION"), "18\n").unwrap();
        let control = root.path().join("package/bin/pg_controldata");
        fs::write(
            &control,
            "#!/bin/sh\nprintf 'Database system identifier: 12345\\n'\n",
        )
        .unwrap();
        fs::set_permissions(control, fs::Permissions::from_mode(0o700)).unwrap();
        let meta = json!({"backup_id":"base-1", "system_identifier":"12345", "pg_major":18,
            "epoch_id":"0123456789abcdef0123456789abcdef", "backup_stop_lsn":"0/100", "post_backup_lsn":"0/200",
            "version":1, "capture_id":"capture-1", "manifest_sha256":codec::file_digest(&manifest).unwrap(),
            "record_contract_sha256":recovery::contract(&config["recovery"]).unwrap(),
            "writer_fence_token":"0123456789abcdef0123456789abcdef", "record_hashes":{"rows":codec::digest(b"actual records\n")},
            "completed_at":now, "wal_segment_bytes":16777216, "timeline":1});
        durable::write_json(&root.path().join("backup/base/base-1.meta.json"), &meta).unwrap();
        durable::write_json(
            &root.path().join("backup/recovery/captures/capture-1.json"),
            &meta,
        )
        .unwrap();
        durable::write_json(
            &root.path().join("backup/recovery/pins/capture-1.json"),
            &meta,
        )
        .unwrap();
        fs::write(
            root.path().join("backup/recovery/PROTOCOL"),
            "source-local-v1\n",
        )
        .unwrap();
        fs::write(
            root.path().join("backup/LAST_SUCCESS"),
            "2026-10-10T12:00:00Z\n",
        )
        .unwrap();
        fs::write(root.path().join("backup/recovery/SELECTED"), "capture-1\n").unwrap();
        Self {
            root,
            config,
            now,
            meta,
        }
    }
    fn p(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }
    fn snapshot_path(&self) -> PathBuf {
        self.p("backup/recovery/snapshots/capture-1.json")
    }
    fn python(&self) -> Value {
        let mut spec = process::CommandSpec::new(vec!["python3".into(), "-B".into(), "-c".into(),
            "import json,sys; from harbor_db import recovery\nc=json.loads(sys.argv[1])\ntry:\n s=recovery.policy(c); d,b=recovery.backup(c,s,int(sys.argv[2])); print(json.dumps({'directory':str(d),'binding':b}))\nexcept Exception as e: print(json.dumps({'error':str(e)}))".into(), self.config.to_string(), self.now.to_string()]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        serde_json::from_slice(&process::execute(&spec).unwrap()).unwrap()
    }
    fn publish_meta(&self, meta: &Value) {
        durable::write_json(&self.p("backup/recovery/captures/capture-1.json"), meta).unwrap();
        fs::copy(
            self.p("backup/recovery/captures/capture-1.json"),
            self.p("backup/recovery/pins/capture-1.json"),
        )
        .unwrap();
    }
}

#[test]
fn invalid_repository_protocol_is_rejected_before_mutation_in_both_readers() {
    let mut f = Fixture::new();
    for protocol in [json!("typo"), Value::Null, json!(1)] {
        f.config["recovery"]["repository_protocol"] = protocol;
        assert!(recovery::policy(&f.config).is_err());
        assert!(f.python().get("error").is_some());
    }
}

#[test]
fn generated_timestamp_is_not_a_legacy_completed_identifier() {
    let f = Fixture::new();
    assert!(
        f.python()["error"]
            .as_str()
            .unwrap()
            .contains("invalid completed backup identifier")
    );
}

#[test]
fn identical_metadata_carriers_preserve_complete_legacy_and_selected_bindings() {
    let mut f = Fixture::new();
    fs::write(f.p("backup/LAST_SUCCESS"), "base-1\n").unwrap();
    let legacy = f.python();
    assert!(legacy.get("error").is_none(), "{legacy}");
    f.config["recovery"]["repository_protocol"] = json!("legacy");
    assert_eq!(f.python(), legacy);
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    // Actual generated-service timestamp remains untouched by capture selection.
    fs::write(f.p("backup/LAST_SUCCESS"), "2026-10-10T12:00:00Z\n").unwrap();
    let selected = recovery_repository::select(&f.config, &f.config["recovery"], f.now).unwrap();
    assert_eq!(selected.metadata(), &f.meta);
    assert_eq!(
        selected.metadata_path(),
        f.p("backup/recovery/captures/capture-1.json")
    );
    assert_eq!(selected.snapshot_path(), f.snapshot_path());
    assert_eq!(selected.binding(), &legacy["binding"]);
    assert_eq!(f.python(), legacy);
    assert_eq!(
        fs::read(f.p("backup/base/base-1.meta.json")).unwrap(),
        fs::read(selected.metadata_path()).unwrap()
    );
    assert!(
        fs::read_dir(f.p("backup/base"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| entry.file_name() == "base-1" || entry.file_name() == "base-1.meta.json")
    );
    selected
        .validate_snapshot(
            f.meta["writer_fence_token"].as_str(),
            &f.meta["record_hashes"],
        )
        .unwrap();
    assert!(
        selected
            .validate_snapshot(None, &f.meta["record_hashes"])
            .is_err()
    );
    assert!(
        selected
            .validate_snapshot(
                f.meta["writer_fence_token"].as_str(),
                &json!({"rows":codec::digest(b"other actual records")})
            )
            .is_err()
    );
}

fn unchanged_files(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let metadata = entry.path().symlink_metadata().unwrap();
        if metadata.is_dir() {
            result.extend(unchanged_files(&entry.path()));
        } else if metadata.is_file() {
            result.push((
                entry.path(),
                fs::read(entry.path()).unwrap(),
                metadata.modified().unwrap(),
            ));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn rejected_unchanged(f: &Fixture) {
    let before = unchanged_files(f.root.path());
    assert!(recovery_repository::select(&f.config, &f.config["recovery"], f.now).is_err());
    assert!(f.python().get("error").is_some());
    assert_eq!(unchanged_files(f.root.path()), before);
}

#[test]
fn capture_drift_and_invalid_metadata_are_rejected_without_writes() {
    let mut f = Fixture::new();
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    for (key, value) in [
        ("version", json!(true)),
        ("capture_id", json!("different")),
        ("backup_id", json!("../base-1")),
        (
            "manifest_sha256",
            json!(codec::digest(b"different manifest")),
        ),
        (
            "record_contract_sha256",
            json!(codec::digest(b"different contract")),
        ),
        ("record_hashes", json!({})),
        (
            "record_hashes",
            json!({"wrong":codec::digest(b"actual records\n")}),
        ),
        ("record_hashes", json!({"rows":"F".repeat(64)})),
        ("writer_fence_token", json!("not-a-token")),
        ("epoch_id", json!("different")),
        ("completed_at", json!(f.now - 7200)),
        ("completed_at", json!(f.now + 7200)),
        ("wal_segment_bytes", json!(3 << 20)),
        ("wal_segment_bytes", json!(true)),
        ("post_backup_lsn", json!("0/100")),
        ("system_identifier", json!("99999")),
        ("pg_major", json!(17)),
    ] {
        let mut metadata = f.meta.clone();
        metadata[key] = value;
        f.publish_meta(&metadata);
        rejected_unchanged(&f);
    }
    f.publish_meta(&f.meta);
    fs::write(f.p("backup/base/base-1/backup_manifest"), "drift").unwrap();
    rejected_unchanged(&f);
}

#[test]
fn unsafe_selectors_and_redirected_carriers_are_rejected_without_writes() {
    let mut f = Fixture::new();
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    for selector in [
        "../capture-1".to_owned(),
        ".capture-1".into(),
        "capture-1.partial".into(),
        "é".into(),
        format!("capture-1{}", " ".repeat(256)),
    ] {
        fs::write(f.p("backup/recovery/SELECTED"), selector).unwrap();
        rejected_unchanged(&f);
    }
    fs::write(f.p("backup/recovery/SELECTED"), "capture-1\n").unwrap();
    for path in [
        "backup/recovery/PROTOCOL",
        "backup/recovery/SELECTED",
        "backup/recovery/pins/capture-1.json",
        "backup/recovery/captures/capture-1.json",
        "backup/base/base-1/backup_manifest",
    ] {
        let original = f.p(path);
        let moved = original.with_extension("saved");
        fs::rename(&original, &moved).unwrap();
        symlink(&moved, &original).unwrap();
        rejected_unchanged(&f);
        fs::remove_file(&original).unwrap();
        fs::rename(&moved, &original).unwrap();
    }
    let original = f.p("backup/recovery/captures");
    let moved = f.p("backup/recovery/redirected");
    fs::rename(&original, &moved).unwrap();
    symlink(&moved, &original).unwrap();
    rejected_unchanged(&f);
}

#[test]
fn protocol_and_exact_immutable_pin_carrier_are_required() {
    let mut f = Fixture::new();
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    for path in [
        "backup/recovery/PROTOCOL",
        "backup/recovery/pins/capture-1.json",
    ] {
        let bytes = fs::read(f.p(path)).unwrap();
        fs::remove_file(f.p(path)).unwrap();
        rejected_unchanged(&f);
        fs::write(f.p(path), bytes).unwrap();
    }
    fs::write(f.p("backup/recovery/PROTOCOL"), "legacy\n").unwrap();
    rejected_unchanged(&f);
    fs::write(f.p("backup/recovery/PROTOCOL"), "source-local-v1\n").unwrap();
    let compact = serde_json::to_vec(&f.meta).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&compact).unwrap(), f.meta);
    fs::write(f.p("backup/recovery/pins/capture-1.json"), compact).unwrap();
    rejected_unchanged(&f);
}

#[test]
fn actual_manifest_ranges_bind_timeline_stop_and_target() {
    let mut f = Fixture::new();
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    for ranges in [
        json!([]),
        json!([{"Timeline":1,"Start-LSN":"0/101","End-LSN":"0/100"}]),
        json!([{"Timeline":1,"Start-LSN":"0/80","End-LSN":"bad"}]),
        json!([{"Timeline":2,"Start-LSN":"0/80","End-LSN":"0/100"}]),
        json!([{"Timeline":true,"Start-LSN":"0/80","End-LSN":"0/100"}]),
        json!([{"Timeline":1,"Start-LSN":"0/80","End-LSN":"0/100"},{"Timeline":2,"Start-LSN":"0/90","End-LSN":"0/100"}]),
        json!([{"Timeline":1,"Start-LSN":"0/80","End-LSN":"0/300"}]),
    ] {
        fs::write(
            f.p("backup/base/base-1/backup_manifest"),
            json!({"WAL-Ranges":ranges}).to_string(),
        )
        .unwrap();
        let mut meta = f.meta.clone();
        meta["manifest_sha256"] =
            json!(codec::file_digest(&f.p("backup/base/base-1/backup_manifest")).unwrap());
        f.publish_meta(&meta);
        rejected_unchanged(&f);
    }
}

impl Fixture {
    fn copied_source(&mut self) {
        self.config["recovery"]["repository_protocol"] = json!("source-local-v1");
        self.config["recovery"]["require_writer_fence"] = json!(false);
        self.config["recovery"]["verify_timeout_seconds"] = json!(10);
        self.config["recovery"]["source_hostname"] = json!("primary-host");
        self.config["data_dir"] = json!(self.p("data"));
        self.config["state_dir"] = json!(self.p("state"));
        self.config["resource"] = json!("repository-reader-fixture");
        fs::write(self.p("data/PG_VERSION"), "18\n").unwrap();
        fs::write(self.p("data/identifier"), "12345").unwrap();
        for path in [
            "backup/locks/mutate",
            "backup/evidence/recovery.lock",
            "backup/evidence/preparation.lock",
        ] {
            fs::write(self.p(path), "").unwrap();
        }
        self.executable("package/bin/pg_verifybackup", "exit 0");
        self.executable("noop", "exit 0");
        fs::write(self.p("rows"), "actual records\n").unwrap();
        fs::write(self.p("primary.json"), json!({"data_dir":self.p("data"), "major":"18", "system_identifier":"12345", "fsync":"on", "full_page_writes":"on", "synchronous_commit":"on", "in_recovery":false}).to_string()).unwrap();
        fs::write(self.p("restored.json"), json!({"data_dir":self.p("restored"), "major":"18", "system_identifier":"12345", "read_only":"on", "in_recovery":false, "replay_lsn":"0/200"}).to_string()).unwrap();
        self.executable("package/bin/psql", &format!("for arg in \"$@\"; do case \"$arg\" in *default_transaction_read_only*) cat '{}'; exit;; *pg_control_system*) cat '{}'; exit;; esac; done; cat '{}'", self.p("restored.json").display(), self.p("primary.json").display(), self.p("rows").display()));
        let selected =
            recovery_repository::select(&self.config, &self.config["recovery"], self.now).unwrap();
        let mut source = selected.binding().clone();
        source["version"] = json!(1);
        source["completed_at"] = json!(self.now);
        source["record_contract_sha256"] = self.meta["record_contract_sha256"].clone();
        source["writer_fence_token"] = self.meta["writer_fence_token"].clone();
        source["records"] = self.meta["record_hashes"].clone();
        durable::write_json(&self.snapshot_path(), &source).unwrap();
        recovery::certify(
            &self.config,
            &self.p("restored"),
            std::path::Path::new("/restore/socket"),
            55432,
            Some(self.now),
            Some("independent-host"),
        )
        .unwrap();
        assert!(recovery::check(&self.config, Some(self.now)).is_ok());
    }
    fn executable(&self, path: &str, body: &str) {
        fs::write(self.p(path), format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(self.p(path), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn preparation(&self) -> Value {
        json!({"readiness_command":[self.p("noop")],"backup_command":[self.p("noop")],"restore_command":[self.p("noop")]})
    }
    fn python_action(&self, action: &str) -> Value {
        let script = "import json,sys; from pathlib import Path; from harbor_db import recovery\nc=json.loads(sys.argv[1]); a=sys.argv[2]; now=int(sys.argv[3]); root=Path(c['data_dir']).parent\ntry:\n if a=='certify': r=recovery.certify(c,root/'restored','/restore/socket',55432,now=now,hostname='independent-host')\n elif a=='snapshot': r=recovery.snapshot(c,'/run/postgresql',5432,now=now)\n elif a=='import': r=recovery.import_off_host(c,root/'incoming.json',now=now)\n elif a=='prepare': r=recovery.prepare(c,{k:[str(root/'noop')] for k in ('readiness_command','backup_command','restore_command')},'/run/postgresql',5432)\n elif a=='live': r=recovery.live_check(c,'/run/postgresql',5432,now=now)\n else: r=recovery.check(c,now=now)\n print(json.dumps(r))\nexcept Exception as e: print(json.dumps({'error':str(e)}))";
        let mut spec = process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            script.into(),
            self.config.to_string(),
            action.into(),
            self.now.to_string(),
        ]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        serde_json::from_slice(&process::execute(&spec).unwrap()).unwrap()
    }
}

#[test]
fn forged_source_records_or_token_never_certify_admit_import_or_prepare() {
    for key in ["records", "writer_fence_token"] {
        let mut f = Fixture::new();
        f.copied_source();
        assert!(f.python_action("certify").get("error").is_none());
        let mut source = durable::read_json(&f.snapshot_path()).unwrap();
        source[key] = if key == "records" {
            fs::write(f.p("rows"), "forged records\n").unwrap();
            json!({"rows":codec::digest(b"forged records\n")})
        } else {
            json!("fedcba9876543210fedcba9876543210")
        };
        durable::write_json(&f.snapshot_path(), &source).unwrap();
        let mut receipt = durable::read_json(&f.p("backup/evidence/receipt.json")).unwrap();
        receipt["records"] = source["records"].clone();
        receipt["snapshot_sha256"] = json!(codec::file_digest(&f.snapshot_path()).unwrap());
        durable::write_json(&f.p("backup/evidence/receipt.json"), &receipt).unwrap();
        durable::write_json(&f.p("incoming.json"), &receipt).unwrap();
        f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
        for action in ["certify", "check", "live", "import", "prepare"] {
            let before = unchanged_files(f.root.path());
            let result = match action {
                "certify" => recovery::certify(
                    &f.config,
                    &f.p("restored"),
                    std::path::Path::new("/restore/socket"),
                    55432,
                    Some(f.now),
                    Some("independent-host"),
                ),
                "check" => recovery::check(&f.config, Some(f.now)),
                "live" => recovery::live_check(
                    &f.config,
                    std::path::Path::new("/run/postgresql"),
                    5432,
                    Some(f.now),
                ),
                "import" => {
                    recovery::import_off_host(&f.config, &f.p("incoming.json"), Some(f.now))
                }
                _ => recovery::prepare_at(
                    &f.config,
                    &f.preparation(),
                    std::path::Path::new("/run/postgresql"),
                    5432,
                    Some(f.now),
                ),
            };
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("source-local capture differs"),
                "{action} / {key}: {error}"
            );
            let python = f.python_action(action);
            assert!(
                python["error"]
                    .as_str()
                    .unwrap()
                    .contains("source-local capture differs"),
                "{action} / {key}: {python}"
            );
            assert_eq!(unchanged_files(f.root.path()), before);
        }
    }
}

#[test]
fn selected_generation_ignores_global_snapshot_and_uncommitted_capture() {
    let mut f = Fixture::new();
    f.copied_source();
    let old_source = fs::read(f.snapshot_path()).unwrap();
    let old_receipt = durable::read_json(&f.p("backup/evidence/receipt.json")).unwrap();
    durable::write_json(
        &f.p("backup/evidence/snapshot.json"),
        &json!({"writer_fence_token":"wrong", "records":{}}),
    )
    .unwrap();
    let mut pending = f.meta.clone();
    pending["capture_id"] = json!("capture-2");
    durable::write_json(&f.p("backup/recovery/captures/capture-2.json"), &pending).unwrap();
    fs::copy(
        f.p("backup/recovery/captures/capture-2.json"),
        f.p("backup/recovery/pins/capture-2.json"),
    )
    .unwrap();
    // Pending publication has no snapshot and has not committed SELECTED.
    let before = unchanged_files(f.root.path());
    assert!(recovery::check(&f.config, Some(f.now)).is_ok());
    assert!(f.python_action("check").get("error").is_none());
    assert_eq!(unchanged_files(f.root.path()), before);
    let native = recovery::certify(
        &f.config,
        &f.p("restored"),
        std::path::Path::new("/restore/socket"),
        55432,
        Some(f.now),
        Some("independent-host"),
    )
    .unwrap();
    assert_eq!(native, old_receipt);
    assert_eq!(f.python_action("certify"), native);
    assert_eq!(fs::read(f.snapshot_path()).unwrap(), old_source);
    assert_eq!(native["snapshot_sha256"], codec::digest(&old_source));
}

#[test]
fn primary_snapshot_retry_returns_frozen_generation_without_republishing() {
    let mut f = Fixture::new();
    f.copied_source();
    f.executable("package/bin/pg_ctl", "exit 3");
    fs::write(f.p("data/postgresql.auto.conf"), "# original\n").unwrap();
    let fence = writer_fence::open_fence(&f.config, "12345").unwrap();
    let record = writer_fence::startup(&f.config).unwrap().unwrap();
    fs::write(f.p("fence.json"),json!({"data_dir":f.config["data_dir"],"major":"18","system_identifier":"12345","hba_file":record["hba_file"],"control_role":"postgres","in_recovery":false,"fsync":"on","full_page_writes":"on","synchronous_commit":"on","preload_libraries":[],"logical_subscriptions":0,"prepared_transactions":0,"other_writers":0}).to_string()).unwrap();
    f.executable("package/bin/psql",&format!("for arg in \"$@\"; do case \"$arg\" in *prepared_transactions*) cat '{}'; exit;; *default_transaction_read_only*) cat '{}'; exit;; *pg_control_system*) cat '{}'; exit;; esac; done; cat '{}'",f.p("fence.json").display(),f.p("restored.json").display(),f.p("primary.json").display(),f.p("rows").display()));
    f.meta["writer_fence_token"] = fence["token"].clone();
    f.meta["epoch_id"] = fence["token"].clone();
    f.meta["completed_at"] = json!(f.now - 10);
    f.publish_meta(&f.meta);
    f.config["recovery"]["require_writer_fence"] = json!(true);
    let selected = recovery_repository::select(&f.config, &f.config["recovery"], f.now).unwrap();
    let mut source = selected.binding().clone();
    source["version"] = json!(1);
    source["completed_at"] = f.meta["completed_at"].clone();
    source["record_contract_sha256"] = f.meta["record_contract_sha256"].clone();
    source["writer_fence_token"] = f.meta["writer_fence_token"].clone();
    source["records"] = f.meta["record_hashes"].clone();
    durable::write_json(&f.snapshot_path(), &source).unwrap();
    let receipt = recovery::certify(
        &f.config,
        &f.p("restored"),
        std::path::Path::new("/restore/socket"),
        55432,
        Some(f.now),
        Some("independent-host"),
    )
    .unwrap();
    let before = unchanged_files(f.root.path());
    let native = recovery::snapshot(
        &f.config,
        std::path::Path::new("/run/postgresql"),
        5432,
        Some(f.now + 30),
    )
    .unwrap();
    assert_eq!(native, source);
    assert_eq!(f.python_action("snapshot"), source);
    assert_eq!(unchanged_files(f.root.path()), before);
    assert_eq!(
        receipt["snapshot_sha256"],
        codec::file_digest(&f.snapshot_path()).unwrap()
    );
    assert!(recovery::check(&f.config, Some(f.now + 30)).is_ok());
    assert!(f.python_action("check").get("error").is_none());
    // Neither language can silently repair an incomplete generation.
    fs::remove_file(f.snapshot_path()).unwrap();
    let before = unchanged_files(f.root.path());
    assert!(
        recovery::snapshot(
            &f.config,
            std::path::Path::new("/run/postgresql"),
            5432,
            Some(f.now)
        )
        .is_err()
    );
    assert!(f.python_action("snapshot").get("error").is_some());
    assert_eq!(unchanged_files(f.root.path()), before);
}

#[test]
fn preparation_without_selected_capture_requires_explicit_producer_without_writes() {
    let mut f = Fixture::new();
    f.copied_source();
    fs::remove_file(f.p("backup/recovery/SELECTED")).unwrap();
    let before = unchanged_files(f.root.path());
    let error = recovery::prepare_at(
        &f.config,
        &f.preparation(),
        std::path::Path::new("/run/postgresql"),
        5432,
        Some(f.now),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("explicitly published producer capture"),
        "{error}"
    );
    let python = f.python_action("prepare");
    assert!(
        python["error"]
            .as_str()
            .unwrap()
            .contains("explicitly published producer capture"),
        "{python}"
    );
    assert_eq!(unchanged_files(f.root.path()), before);
}

#[test]
fn source_local_primary_snapshot_requires_fence_before_evidence_mutation() {
    let mut f = Fixture::new();
    f.config["recovery"]["repository_protocol"] = json!("source-local-v1");
    for fence in [None, Some(false)] {
        if let Some(fence) = fence {
            f.config["recovery"]["require_writer_fence"] = json!(fence);
        }
        let before = unchanged_files(f.root.path());
        assert!(
            recovery::snapshot(
                &f.config,
                std::path::Path::new("/run/postgresql"),
                5432,
                Some(f.now)
            )
            .unwrap_err()
            .to_string()
            .contains("require the writer fence")
        );
        let mut spec = process::CommandSpec::new(vec!["python3".into(), "-B".into(), "-c".into(), "import json,sys; from harbor_db import recovery\ntry: recovery.snapshot(json.loads(sys.argv[1]),'/run/postgresql',5432)\nexcept Exception as e: print(str(e))".into(), f.config.to_string()]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        assert!(
            String::from_utf8(process::execute(&spec).unwrap())
                .unwrap()
                .contains("require the writer fence")
        );
        assert_eq!(unchanged_files(f.root.path()), before);
        // Independent certification policy intentionally permits false.
        assert!(recovery::policy(&f.config).is_ok());
    }
}
