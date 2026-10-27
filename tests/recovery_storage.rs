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

#[test]
fn recovery_machine_bindings_reject_hostname_only_remote_evidence_in_both_engines() {
    let f = Fixture::new();
    f.accepted();
    let source = f.source();
    let original = f.receipt();
    let binding = json!({"recovery_target_lsn":"0/200"});
    for machine in [
        source["source_machine_sha256"].clone(),
        json!(null),
        json!(""),
        json!("A".repeat(64)),
        json!(17),
    ] {
        let mut receipt = original.clone();
        receipt["executor_host"] = json!("hostname-only-remote");
        receipt["executor_machine_sha256"] = machine;
        assert!(
            recovery::validate_receipt(
                &f.config,
                &f.config["recovery"],
                &source,
                &binding,
                &receipt,
                "off-host restore acceptance"
            )
            .is_err()
        );
        let script = "import json,sys; from harbor_db import recovery\nc,s,b,r=map(json.loads,sys.argv[1:])\ntry: recovery.validate_receipt(c,c['recovery'],s,b,r,'off-host restore acceptance')\nexcept ValueError as e: assert 'independent machine' in str(e)\nelse: raise AssertionError('hostname-only independence admitted')";
        let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            script.into(),
            f.config.to_string(),
            source.to_string(),
            binding.to_string(),
            receipt.to_string(),
        ]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        harbor_db::storage::process::execute(&spec).unwrap();
    }
    let mut remote = original;
    remote["executor_host"] = json!("independent-host");
    remote["executor_machine_sha256"] = json!("0".repeat(64));
    recovery::validate_receipt(
        &f.config,
        &f.config["recovery"],
        &source,
        &binding,
        &remote,
        "off-host restore acceptance",
    )
    .unwrap();
    remote["source_machine_sha256"] = json!("1".repeat(64));
    assert!(
        recovery::validate_receipt(
            &f.config,
            &f.config["recovery"],
            &source,
            &binding,
            &remote,
            "off-host restore acceptance"
        )
        .is_err()
    );
}

#[test]
fn uts_hostname_change_cannot_certify_an_independent_machine_in_both_engines() {
    const NAME: &str = "uts_hostname_change_cannot_certify_an_independent_machine_in_both_engines";
    if std::env::var_os("HARBOR_RECOVERY_UTS_TEST_CHILD").is_none() {
        let child = std::process::Command::new("unshare")
            .args(["--user","--map-root-user","--uts","--","python3","-B","-c", "import os,socket,sys; socket.sethostname('hostname-only-remote'); os.execv(sys.argv[1],[sys.argv[1],'--exact',sys.argv[2],'--nocapture'])"])
            .arg(std::env::current_exe().unwrap()).arg(NAME)
            .env("HARBOR_RECOVERY_UTS_TEST_CHILD","1").output().unwrap();
        assert!(
            child.status.success()
                && String::from_utf8_lossy(&child.stdout).contains("test result: ok. 1 passed;"),
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }
    let mut f = Fixture::new();
    f.snapshot();
    f.config["recovery"]["off_host_receipt_file"] = f.config["recovery"]["receipt_file"].clone();
    let error = recovery::certify(
        &f.config,
        &f.p("restored"),
        Path::new("/restore/socket"),
        55432,
        Some(f.now),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("independent machine"), "{error}");
    assert!(!f.p("backup/evidence/recovery.json").exists());
    let script = "import json,socket,sys; from pathlib import Path; from harbor_db import recovery\nassert socket.gethostname()=='hostname-only-remote'\nif sys.argv[3]: recovery.application_backup.machine=lambda:recovery.application_backup.digest(sys.argv[3])\nc=json.loads(sys.argv[1])\ntry: recovery.certify(c,Path(c['data_dir']).parent/'restored','/restore/socket',55432,now=int(sys.argv[2]))\nexcept ValueError as e: assert 'independent machine' in str(e)\nelse: raise AssertionError('UTS name established off-host independence')";
    let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        f.config.to_string(),
        f.now.to_string(),
        option_env!("HARBOR_DB_TEST_MACHINE_ID")
            .unwrap_or("")
            .into(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&spec).unwrap();
    assert!(!f.p("backup/evidence/recovery.json").exists());
}

fn evidence_preflight(config: &Value, now: Option<i64>) -> harbor_db::storage::Result<Value> {
    Ok(recovery::evidence_admission(config, now, false, None, 5432)?.result)
}
fn executable(path: &Path, script: &str) {
    fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
struct Fixture {
    root: tempfile::TempDir,
    config: Value,
    now: i64,
}

#[test]
fn local_acceptance_cannot_reuse_an_independent_host_receipt_in_both_engines() {
    let f = Fixture::new();
    f.accepted();
    let mut receipt = f.receipt();
    let source = f.source();
    let binding = json!({"recovery_target_lsn":"0/200"});
    for executor in [json!("independent-host"), json!(null), json!("")] {
        receipt["executor_host"] = executor;
        assert!(
            recovery::validate_receipt(
                &f.config,
                &f.config["recovery"],
                &source,
                &binding,
                &receipt,
                "restore acceptance"
            )
            .is_err()
        );
    }
    receipt["executor_host"] = json!("independent-host");
    receipt["executor_machine_sha256"] = json!("0".repeat(64));
    recovery::validate_receipt(
        &f.config,
        &f.config["recovery"],
        &source,
        &binding,
        &receipt,
        "off-host restore acceptance",
    )
    .unwrap();
    durable::write_json(&f.p("backup/evidence/recovery.json"), &receipt).unwrap();
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    let script = r#"import json,sys
from harbor_db import recovery
c,source,binding,receipt=map(json.loads,sys.argv[1:])
for executor in ('independent-host',None,''):
    try: recovery.validate_receipt(c,c['recovery'],source,binding,receipt|{'executor_host':executor},'restore acceptance')
    except ValueError: pass
    else: raise AssertionError('local evidence reused a remote receipt')
recovery.validate_receipt(c,c['recovery'],source,binding,receipt|{'executor_host':'primary-host','executor_machine_sha256':source['source_machine_sha256']},'restore acceptance')
recovery.validate_receipt(c,c['recovery'],source,binding,receipt,'off-host restore acceptance')
"#;
    let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        f.config.to_string(),
        source.to_string(),
        binding.to_string(),
        receipt.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&spec).unwrap();
}
#[test]
fn local_and_off_host_receipt_paths_cannot_alias_in_admission() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.snapshot();
    f.config["recovery"]["off_host_receipt_file"] = f.config["recovery"]["receipt_file"].clone();
    let error = recovery::policy(&f.config).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("local and off-host recovery receipts must be distinct"),
        "{error}"
    );
    let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
        "python3".into(), "-B".into(), "-c".into(),
        "import json,sys; from harbor_db import recovery; c=json.loads(sys.argv[1]);\ntry: recovery.policy(c)\nexcept ValueError as e: assert 'local and off-host recovery receipts must be distinct' in str(e)\nelse: raise AssertionError('aliased receipt paths admitted')".into(),
        f.config.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&spec).unwrap();
    assert!(!f.p("backup/evidence/recovery.json").exists());
    assert!(
        recovery::certify(
            &f.config,
            &f.p("restored"),
            Path::new("/restore/socket"),
            55432,
            Some(f.now),
            Some("primary-host"),
        )
        .is_err()
    );
    assert!(!f.p("backup/evidence/recovery.json").exists());
    let spoofed = recovery::certify(
        &f.config,
        &f.p("restored"),
        Path::new("/restore/socket"),
        55432,
        Some(f.now),
        Some("independent-host"),
    )
    .unwrap_err();
    assert!(spoofed.to_string().contains("independent machine"));
    assert!(!f.p("backup/evidence/recovery.json").exists());
    f.config["recovery"]["off_host_receipt_file"] = json!(null);
    let mut remote = f.certify();
    f.config["recovery"]["off_host_receipt_file"] = f.config["recovery"]["receipt_file"].clone();
    remote["executor_host"] = json!("independent-host");
    remote["executor_machine_sha256"] = json!("0".repeat(64));
    durable::write_json(&f.p("backup/evidence/recovery.json"), &remote).unwrap();
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    // Shared output selection belongs only to certification. Admission reads
    // the independent execution from its own, distinct path.
    f.config["recovery"]["receipt_file"] = json!(f.p("backup/evidence/local.json"));
    f.certify();
    assert_eq!(
        recovery::evidence_check(&f.config, Some(f.now)).unwrap()["off_host"],
        "independent-host"
    );
}

#[test]
fn source_local_evidence_cannot_replace_repository_control_paths_in_both_engines() {
    evidence_cannot_replace_repository_controls(Some("source-local-v1"));
}

#[test]
fn legacy_evidence_cannot_replace_repository_control_paths_in_both_engines() {
    evidence_cannot_replace_repository_controls(None);
    evidence_cannot_replace_repository_controls(Some("legacy"));
}

fn evidence_cannot_replace_repository_controls(protocol: Option<&str>) {
    let mut f = Fixture::new();
    if let Some(protocol) = protocol {
        f.config["recovery"]["repository_protocol"] = json!(protocol);
    }
    let original = f.config.clone();
    for key in ["snapshot_file", "receipt_file", "off_host_receipt_file"] {
        for suffix in [
            "recovery/SELECTED",
            "recovery/snapshots/point.json",
            "base/point/backup_manifest",
            "wal/segment",
            "locks/mutate",
            "BACKUP_LOCK",
            "LAST_SUCCESS",
            "evidence/recovery.lock",
            "evidence/preparation.lock",
            "evidence/preparation.json",
            "evidence/../recovery/SELECTED",
            "evidence//receipt.json",
        ] {
            f.config = original.clone();
            f.config["recovery"][key] = json!(f.p(&format!("backup/{suffix}")));
            let error = recovery::policy(&f.config).unwrap_err();
            assert!(
                error.to_string().contains("repository control"),
                "{key}/{suffix}: {error}"
            );
            let script = r#"import json,sys
from harbor_db import recovery
try: recovery.policy(json.loads(sys.argv[1]))
except ValueError as error: assert 'repository control' in str(error)
else: raise AssertionError('producer control path admitted as evidence')
"#;
            let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
                "python3".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                f.config.to_string(),
            ]);
            spec.environment = Some(std::collections::BTreeMap::from([
                (
                    "PYTHONPATH".into(),
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                ),
                ("PATH".into(), std::env::var("PATH").unwrap()),
            ]));
            harbor_db::storage::process::execute(&spec).unwrap();
        }
    }
    recovery::policy(&original).unwrap();
    assert!(!f.p("backup/evidence/recovery.lock").exists());
    assert!(!f.p("backup/evidence/preparation.lock").exists());
    assert!(!f.p("backup/evidence/preparation.json").exists());
    let preparation = f.preparation();
    let anchor = f.p("backup/evidence/preparation.lock");
    let journal = f.p("backup/evidence/preparation.json");
    let _lease = durable::lock(&anchor, false, true).unwrap();
    fs::write(&journal, b"retained preparation binding").unwrap();
    use std::os::unix::fs::MetadataExt;
    let identities = [&anchor, &journal].map(|path| {
        let info = path.metadata().unwrap();
        (info.dev(), info.ino(), fs::read(path).unwrap())
    });
    for key in ["snapshot_file", "receipt_file", "off_host_receipt_file"] {
        for selected in [&anchor, &journal] {
            f.config = original.clone();
            f.config["recovery"][key] = json!(selected);
            assert!(
                f.prepare(&preparation)
                    .unwrap_err()
                    .to_string()
                    .contains("repository control")
            );
            let script = r#"import json,sys
from harbor_db import recovery
try: recovery.prepare(json.loads(sys.argv[1]),json.loads(sys.argv[2]),'/run/postgresql',5432)
except ValueError as error: assert 'repository control' in str(error),str(error)
else: raise AssertionError('preparation control collision admitted')
"#;
            let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
                "python3".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                f.config.to_string(),
                preparation.to_string(),
            ]);
            spec.environment = Some(std::collections::BTreeMap::from([
                (
                    "PYTHONPATH".into(),
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                ),
                ("PATH".into(), std::env::var("PATH").unwrap()),
            ]));
            harbor_db::storage::process::execute(&spec).unwrap();
            let observed = [&anchor, &journal].map(|path| {
                let info = path.metadata().unwrap();
                (info.dev(), info.ino(), fs::read(path).unwrap())
            });
            assert_eq!(observed, identities);
            assert!(!f.p("preparation.log").exists());
        }
    }
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
                "case \" $* \" in *' --skip-checksums '*) printf 'skip\\n';; *) printf 'verify\\n';; esac >> '{}'; test ! -e '{}'",
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
                "for arg in \"$@\"; do case \"$arg\" in *prepared_transactions*) cat '{}'; exit;; *current_setting*default_transaction_read_only*) cat '{}'; exit;; *pg_control_system*) cat '{}'; exit;; esac; done; cat '{}'",
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

#[test]
fn recovery_evidence_cannot_replace_postgres_storage_in_both_engines() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    fs::create_dir_all(f.p("data/global")).unwrap();
    fs::create_dir(f.p("startup")).unwrap();
    f.config["startup_inhibition"] = json!({"state_dir":f.p("startup")});
    for file in [
        "data/global/pg_control",
        "state/identity.json",
        "state/writer-fence.json",
        "startup/policy.json",
    ] {
        fs::write(f.p(file), b"retained primary or authority state").unwrap();
    }
    let retained = [
        "data/PG_VERSION",
        "data/global/pg_control",
        "state/identity.json",
        "state/writer-fence.json",
        "startup/policy.json",
    ];
    let before: Vec<_> = retained
        .iter()
        .map(|file| {
            let path = f.p(file);
            let info = path.metadata().unwrap();
            (info.dev(), info.ino(), fs::read(path).unwrap())
        })
        .collect();
    let original = f.config.clone();
    for protocol in [None, Some("legacy"), Some("source-local-v1")] {
        for key in [
            "backup_root",
            "snapshot_file",
            "receipt_file",
            "off_host_receipt_file",
        ] {
            for target in [
                "data",
                "data/PG_VERSION",
                "data/global/pg_control",
                "state",
                "state/identity.json",
                "state/writer-fence.json",
                "startup",
                "startup/policy.json",
                "",
            ] {
                f.config = original.clone();
                if let Some(protocol) = protocol {
                    f.config["recovery"]["repository_protocol"] = json!(protocol);
                }
                f.config["recovery"][key] = json!(if target.is_empty() {
                    f.root.path().to_owned()
                } else {
                    f.p(target)
                });
                let error = recovery::policy(&f.config).unwrap_err();
                assert!(
                    error.to_string().contains("PostgreSQL storage"),
                    "{key}/{target}: {error}"
                );
                let error =
                    recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now))
                        .unwrap_err();
                assert!(error.to_string().contains("PostgreSQL storage"), "{error}");
                let script = r#"import json,sys
from harbor_db import recovery
config=json.loads(sys.argv[1])
for operation in (lambda: recovery.policy(config), lambda: recovery.snapshot(config,'/run/postgresql',5432,now=int(sys.argv[2]))):
 try: operation()
 except ValueError as error: assert 'PostgreSQL storage' in str(error),str(error)
 else: raise AssertionError('primary storage admitted as evidence')
"#;
                let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
                    "python3".into(),
                    "-B".into(),
                    "-c".into(),
                    script.into(),
                    f.config.to_string(),
                    f.now.to_string(),
                ]);
                spec.environment = Some(std::collections::BTreeMap::from([
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                ]));
                harbor_db::storage::process::execute(&spec).unwrap();
                let after: Vec<_> = retained
                    .iter()
                    .map(|file| {
                        let path = f.p(file);
                        let info = path.metadata().unwrap();
                        (info.dev(), info.ino(), fs::read(path).unwrap())
                    })
                    .collect();
                assert_eq!(before, after);
                assert!(!f.p("backup/evidence/records.json").exists());
                assert!(!f.p("verify.log").exists());
            }
        }
    }
    f.config = original;
    for key in [
        "backup_root",
        "snapshot_file",
        "receipt_file",
        "off_host_receipt_file",
    ] {
        let mut independent = f.config.clone();
        independent["recovery"][key] = json!(f.p("data-sibling/receipt.json"));
        recovery::policy(&independent).unwrap();
    }
    for file in [
        "state/identity.json",
        "state/writer-fence.json",
        "startup/policy.json",
    ] {
        fs::remove_file(f.p(file)).unwrap();
    }
    f.snapshot();
}

#[test]
fn primary_configuration_links_are_protected_without_admitting_restore_redirects_in_both_engines() {
    let f = Fixture::new();
    f.accepted();
    let configuration = f.p("generated-postgresql.conf");
    fs::write(&configuration, b"generated immutable configuration").unwrap();
    symlink(&configuration, f.p("data/postgresql.conf")).unwrap();
    recovery::certify(
        &f.config,
        &f.p("restored"),
        Path::new("/socket"),
        55432,
        Some(f.now),
        Some("primary-host"),
    )
    .unwrap();
    fs::hard_link(&configuration, f.p("restored/shared-config")).unwrap();
    assert!(
        recovery::certify(
            &f.config,
            &f.p("restored"),
            Path::new("/socket"),
            55432,
            Some(f.now),
            Some("primary-host")
        )
        .unwrap_err()
        .to_string()
        .contains("shared storage")
    );
    fs::remove_file(f.p("restored/shared-config")).unwrap();
    symlink(&configuration, f.p("restored/redirected-config")).unwrap();
    assert!(
        recovery::certify(
            &f.config,
            &f.p("restored"),
            Path::new("/socket"),
            55432,
            Some(f.now),
            Some("primary-host")
        )
        .unwrap_err()
        .to_string()
        .contains("redirected or special")
    );
    let script = r#"import json,os,sys
from pathlib import Path
from harbor_db import recovery
c=json.loads(sys.argv[1]); root=Path(c['data_dir']).parent
restored=root/'restored'; configuration=root/'generated-postgresql.conf'
try: recovery.isolated_restore(root/'data',root/'data',root/'backup/base/base-1')
except ValueError as error: assert 'shared storage' in str(error)
else: raise AssertionError('primary root alias admitted')
try: recovery.isolated_restore(restored,root/'data',root/'backup/base/base-1')
except ValueError as error: assert 'redirected or special' in str(error)
else: raise AssertionError('redirected restoration admitted')
(restored/'redirected-config').unlink()
recovery.isolated_restore(restored,root/'data',root/'backup/base/base-1')
locked=root/'executor-postgresql'; locked.mkdir(); locked.chmod(0)
try:
    recovery.isolated_restore(restored,locked/'17',root/'backup/base/base-1',independent=True)
    if os.geteuid() != 0:
        try: recovery.isolated_restore(restored,locked/'17',root/'backup/base/base-1')
        except PermissionError: pass
        else: raise AssertionError('local certifier skipped an unsearchable primary')
finally: locked.chmod(0o700)
(restored/'shared-config').hardlink_to(configuration)
try: recovery.isolated_restore(restored,root/'data',root/'backup/base/base-1')
except ValueError as error: assert 'shared storage' in str(error)
else: raise AssertionError('shared primary configuration admitted')
"#;
    let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        f.config.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&spec).unwrap();
}

fn python_recovery(f: &Fixture, action: &str, preparation: &Value) -> Value {
    use harbor_db::storage::process;
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import recovery
config,action,preparation,now=json.loads(sys.argv[1]),sys.argv[2],json.loads(sys.argv[3]),int(sys.argv[4])
if sys.argv[5]: recovery.application_backup.machine=lambda:recovery.application_backup.digest(sys.argv[5])
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
        option_env!("HARBOR_DB_TEST_MACHINE_ID")
            .unwrap_or("")
            .into(),
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
            recovery::evidence_check(&f.config, Some(f.now)).unwrap()
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
    let mut complete = settings.clone();
    complete["backup_root"] = json!("/backups");
    assert!(recovery::policy(&json!({"recovery":complete})).is_ok());
    complete["backup_root"] = json!(null);
    assert!(recovery::policy(&json!({"recovery":complete})).is_err());
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
        recovery::evidence_check(&f.config, Some(f.now)).unwrap()["backup_id"],
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
fn backup_checksums_run_once_per_snapshot_and_again_off_host() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    // The snapshot verifies the bytes; the local drill and admission reuse them.
    f.accepted();
    recovery::evidence_check(&f.config, Some(f.now)).unwrap();
    // An off-host certifier's copy crossed hosts: full checksums again.
    let mut source = f.source();
    source["source_machine_sha256"] = json!("0".repeat(64));
    durable::write_json(&f.p("backup/evidence/records.json"), &source).unwrap();
    recovery::certify(
        &f.config,
        &f.p("restored"),
        Path::new("/restore/socket"),
        55432,
        Some(f.now),
        Some("independent-host"),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(f.p("verify.log")).unwrap(),
        "verify\nskip\nskip\nverify\n"
    );
}

#[test]
fn concurrent_record_checks_keep_each_digest_with_its_check() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.config["recovery"]["record_checks"] = json!(
        (0..6)
            .map(|n| json!({"name": format!("c{n}"), "database": "app", "sql": format!("SELECT check {n}")}))
            .collect::<Vec<_>>()
    );
    // Earlier checks answer later, so completion order is not check order.
    let psql = f.p("package/bin/psql");
    let script = fs::read_to_string(&psql).unwrap();
    executable(
        &psql,
        &format!(
            "for arg in \"$@\"; do case \"$arg\" in 'PREPARE harbor_db_recovery_query AS SELECT check '*) n=${{arg##* }}; sleep 0.$((6 - n)); echo \"$n\"; exit;; esac; done\n{}",
            script.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    );
    let records = f.snapshot()["records"].clone();
    for n in 0..6 {
        assert_eq!(
            records[format!("c{n}")],
            json!(codec::digest(format!("{n}\n").as_bytes())),
            "{records}"
        );
    }
}

#[test]
fn record_checks_still_complete_with_a_single_free_connection() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.config["recovery"]["record_checks"] = json!(
        (0..6)
            .map(|n| json!({"name": format!("c{n}"), "database": "app", "sql": format!("SELECT check {n}")}))
            .collect::<Vec<_>>()
    );
    // A primary with one free connection refuses every concurrent session.
    let psql = f.p("package/bin/psql");
    let script = fs::read_to_string(&psql).unwrap();
    executable(
        &psql,
        &format!(
            "for arg in \"$@\"; do case \"$arg\" in 'PREPARE harbor_db_recovery_query AS SELECT check '*) mkdir '{}' 2>/dev/null || exit 2; sleep 0.1; echo \"${{arg##* }}\"; rmdir '{}'; exit;; esac; done\n{}",
            f.p("connection").display(),
            f.p("connection").display(),
            script.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    );
    let records = f.snapshot()["records"].clone();
    for n in 0..6 {
        assert_eq!(
            records[format!("c{n}")],
            json!(codec::digest(format!("{n}\n").as_bytes())),
            "{records}"
        );
    }
}

#[test]
fn executor_certification_replaces_the_local_drill_only_when_configured() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.accepted();
    let mut independent = f.receipt();
    independent["executor_host"] = json!("independent-host");
    independent["executor_machine_sha256"] = json!("0".repeat(64));
    fs::remove_file(f.p("backup/evidence/recovery.json")).unwrap();
    let prep = f.preparation();
    executable(
        &f.p("restore-command"),
        &format!(
            "printf 'restore\\n' >> '{}'",
            f.p("preparation.log").display()
        ),
    );
    // Without an executor the local drill runs and its receipt is required.
    assert!(f.prepare(&prep).is_err());
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    // With one, its certification alone admits: no drill, no local receipt.
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    durable::write_json(&f.p("backup/evidence/off-host.json"), &independent).unwrap();
    assert_eq!(f.prepare(&prep).unwrap()["off_host"], "independent-host");
    assert_eq!(
        recovery::evidence_check(&f.config, Some(f.now)).unwrap()["off_host"],
        "independent-host"
    );
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nrestore\nreadiness\n"
    );
    assert!(!f.p("backup/evidence/recovery.json").exists());
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
    remote["executor_machine_sha256"] = json!("0".repeat(64));
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
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    assert!(!f.p("backup/evidence/recovery.lock").exists());
    f.snapshot();
    assert!(evidence_preflight(&f.config, Some(f.now)).is_err());
    f.certify();
    fs::remove_file(f.p("backup/evidence/recovery.lock")).unwrap();
    assert!(evidence_preflight(&f.config, Some(f.now)).is_err());
    assert!(!f.p("backup/evidence/recovery.lock").exists());
    fs::remove_file(f.p("backup/locks/mutate")).unwrap();
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
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
        evidence_preflight(&f.config, Some(f.now)).unwrap()["status"],
        "preflight-ready"
    );
    assert_eq!(fs::read(f.p("verify.log")).unwrap(), before);
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    for now in [f.now - 1, f.now + 3601] {
        assert!(evidence_preflight(&f.config, Some(now)).is_err());
    }
    let mut bad = f.config.clone();
    bad["recovery"]["record_checks"][0]["sql"] = json!("SELECT different_records");
    assert!(evidence_preflight(&bad, Some(f.now)).is_err());
    // Routine preflight needs only a fresh bound backup, not matching evidence.
    assert_eq!(
        recovery::preflight(&bad, Some(f.now)).unwrap()["status"],
        "preflight-ready"
    );
    for now in [f.now - 1, f.now + 3601] {
        assert!(recovery::preflight(&f.config, Some(now)).is_err());
    }
}

#[test]
fn routine_admission_survives_a_newer_base_but_transitions_require_its_evidence() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    let base = f.p("backup/base/base-2");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("PG_VERSION"), "18\n").unwrap();
    fs::write(base.join("identifier"), "12345").unwrap();
    fs::write(base.join("backup_manifest"), "{\"WAL-Ranges\": [1]}\n").unwrap();
    durable::write_json(&f.p("backup/base/base-2.meta.json"),&json!({"backup_id":"base-2","system_identifier":"12345","pg_major":18,"epoch_id":"epoch-2","backup_stop_lsn":"0/300","post_backup_lsn":"0/400"})).unwrap();
    fs::write(f.p("backup/LAST_SUCCESS"), "base-2\n").unwrap();
    for admitted in [
        recovery::check(&f.config, Some(f.now)).unwrap(),
        recovery::preflight(&f.config, Some(f.now)).unwrap(),
    ] {
        assert_eq!(admitted["backup_id"], "base-2");
    }
    for err in [
        recovery::evidence_check(&f.config, Some(f.now)).unwrap_err(),
        evidence_preflight(&f.config, Some(f.now)).unwrap_err(),
        recovery::live_check(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now))
            .unwrap_err(),
    ] {
        assert!(err.to_string().contains("different backup"), "{err}");
    }
    assert!(recovery::check(&f.config, Some(f.now + 3601)).is_err());
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
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
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
        assert!(
            recovery::evidence_check(&f.config, Some(f.now)).is_err(),
            "{target}"
        );
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
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
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
                Some("primary-host")
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
            Some("primary-host")
        )
        .is_err()
    );
}

#[test]
fn restored_clusters_cannot_share_primary_or_selected_backup_inodes_in_both_engines() {
    let _workers = WORKERS.lock().unwrap();
    for python in [false, true] {
        for protected in ["data", "backup/base/base-1"] {
            let f = Fixture::new();
            f.snapshot();
            fs::create_dir_all(f.p(&format!("{protected}/base/1"))).unwrap();
            fs::create_dir_all(f.p("restored/base/9")).unwrap();
            let source = f.p(&format!("{protected}/base/1/123"));
            let alias = f.p("restored/base/9/456");
            fs::write(&source, b"authoritative acknowledged record").unwrap();
            fs::hard_link(&source, &alias).unwrap();
            let before = fs::read(&source).unwrap();
            let result = if python {
                let mut spec=harbor_db::storage::process::CommandSpec::new(vec!["python3".into(),"-B".into(),"-c".into(),
                    "import json,sys\nfrom pathlib import Path\nfrom harbor_db import recovery\nif sys.argv[3]: recovery.application_backup.machine=lambda:recovery.application_backup.digest(sys.argv[3])\nc=json.loads(sys.argv[1])\ntry: recovery.certify(c,Path(c['data_dir']).parent/'restored','/restore/socket',55432,now=int(sys.argv[2]),hostname='primary-host')\nexcept ValueError as e: print(json.dumps({'error':str(e)}))\nelse: print(json.dumps({'accepted':True}))".into(),f.config.to_string(),f.now.to_string(),option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or("").into()]);
                spec.environment = Some(std::collections::BTreeMap::from([
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                ]));
                serde_json::from_slice::<Value>(
                    &harbor_db::storage::process::execute(&spec).unwrap(),
                )
                .unwrap()
            } else {
                match recovery::certify(
                    &f.config,
                    &f.p("restored"),
                    Path::new("/restore/socket"),
                    55432,
                    Some(f.now),
                    Some("primary-host"),
                ) {
                    Ok(value) => value,
                    Err(error) => json!({"error":error.to_string()}),
                }
            };
            assert!(
                result["error"]
                    .as_str()
                    .is_some_and(|s| s.contains("shared storage")),
                "python={python}, protected={protected}: {result}"
            );
            assert!(!f.p("backup/evidence/recovery.json").exists());
            assert_eq!(fs::read(&source).unwrap(), before);
            fs::remove_file(&alias).unwrap();
            fs::copy(&source, &alias).unwrap();
            if python {
                python_recovery(&f, "certify", &json!({}));
            } else {
                f.certify();
            }
        }
    }
}

#[test]
fn primary_and_selected_backup_cannot_share_inodes_even_with_an_independent_restore_in_both_engines()
 {
    let _workers = WORKERS.lock().unwrap();
    let mut outcomes = Vec::new();
    for python in [false, true] {
        for mounted in [false, true] {
            let f = Fixture::new();
            f.snapshot();
            fs::create_dir_all(f.p("data/primary-records")).unwrap();
            fs::write(f.p("data/primary-records/row"), b"acknowledged records").unwrap();
            if mounted {
                fs::create_dir(f.p("data/empty-directory")).unwrap();
                fs::create_dir(f.p("backup/base/base-1/shared-directory")).unwrap();
            } else {
                fs::hard_link(
                    f.p("data/primary-records/row"),
                    f.p("backup/base/base-1/shared-row"),
                )
                .unwrap();
            }
            let rejected = if python || mounted {
                fs::write(f.p("config.json"), f.config.to_string()).unwrap();
                let mut argv = if python {
                    vec!["python3".into(),"-B".into(),"-c".into(),"import json,sys;from pathlib import Path;from harbor_db import recovery\nif sys.argv[2]: recovery.application_backup.machine=lambda:recovery.application_backup.digest(sys.argv[2])\nc=json.loads(sys.argv[1]);recovery.certify(c,Path(c['data_dir']).parent/'restored','/restore/socket',55432,hostname='primary-host')".into(),f.config.to_string(),option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or("").into()]
                } else {
                    vec![
                        env!("CARGO_BIN_EXE_harbor-db-postgres").into(),
                        "--config".into(),
                        f.p("config.json").display().to_string(),
                        "certify-recovery".into(),
                        "--socket-dir".into(),
                        "/restore/socket".into(),
                        "--port".into(),
                        "55432".into(),
                        "--data-dir".into(),
                        f.p("restored").display().to_string(),
                    ]
                };
                if mounted {
                    let mut namespace = vec![
                        "unshare".into(),
                        "--user".into(),
                        "--map-root-user".into(),
                        "--mount".into(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        "mount --bind -- \"$1\" \"$2\" || exit; shift 2; exec \"$@\"".into(),
                        "shared-backup-fixture".into(),
                        f.p("data/empty-directory").display().to_string(),
                        f.p("backup/base/base-1/shared-directory")
                            .display()
                            .to_string(),
                    ];
                    namespace.append(&mut argv);
                    argv = namespace;
                }
                let mut spec = harbor_db::storage::process::CommandSpec::new(argv);
                spec.environment = Some(std::collections::BTreeMap::from([
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                ]));
                // Preserve diagnostics here; production storage workers deliberately discard them.
                let mut command = std::process::Command::new(&spec.argv[0]);
                command
                    .args(&spec.argv[1..])
                    .env_clear()
                    .envs(spec.environment.as_ref().unwrap())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let output = harbor_db::storage::process::spawn(&mut command)
                    .unwrap()
                    .wait_with_output()
                    .unwrap();
                assert!(
                    !output.status.success()
                        && String::from_utf8_lossy(&output.stderr)
                            .contains("selected backup aliases shared storage"),
                    "python={python}, mounted={mounted}, {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                true
            } else {
                recovery::certify(
                    &f.config,
                    &f.p("restored"),
                    Path::new("/restore/socket"),
                    55432,
                    Some(f.now),
                    Some("primary-host"),
                )
                .is_err()
            };
            outcomes.push((
                python,
                mounted,
                rejected,
                !f.p("backup/evidence/recovery.json").exists(),
            ));
            assert_eq!(
                fs::read(f.p("data/primary-records/row")).unwrap(),
                b"acknowledged records"
            );
        }
    }
    assert!(outcomes.iter().all(|(_, _, a, b)| *a && *b), "{outcomes:?}");
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
        assert!(
            recovery::evidence_check(&f.config, Some(f.now)).is_err(),
            "{key}"
        );
    }
    durable::write_json(&f.p("backup/evidence/recovery.json"), &good).unwrap();
    recovery::evidence_check(&f.config, Some(f.now)).unwrap();
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
        assert!(evidence_preflight(&f.config, Some(f.now)).is_err());
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
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
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
    receipt["executor_machine_sha256"] = json!("0".repeat(64));
    durable::write_json(&incoming, &receipt).unwrap();
    recovery::import_off_host(&f.config, &incoming, Some(f.now)).unwrap();
    assert_eq!(
        recovery::evidence_check(&f.config, Some(f.now)).unwrap()["off_host"],
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
        assert!(
            recovery::evidence_check(&f.config, Some(f.now)).is_err(),
            "{name}"
        );
    }
    let f = Fixture::new();
    f.accepted();
    fs::rename(f.p("backup/evidence"), f.p("evidence-saved")).unwrap();
    symlink(f.p("evidence-saved"), f.p("backup/evidence")).unwrap();
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
}

#[test]
fn backup_and_evidence_leases_exclude_mutation_through_authority_publication() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.accepted();
    for anchor in ["backup/locks/mutate", "backup/evidence/recovery.lock"] {
        let lease = durable::lock(&f.p(anchor), false, false).unwrap();
        assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
        assert!(
            recovery::snapshot(&f.config, Path::new("/run/postgresql"), 5432, Some(f.now)).is_err()
        );
        drop(lease);
    }
    let accepted = recovery::evidence_admission(&f.config, Some(f.now), true, None, 5432).unwrap();
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
    let accepted = recovery::evidence_admission(&f.config, Some(f.now), true, None, 5432).unwrap();
    assert!(writer_fence::close_fence(&f.config, &token).is_err());
    drop(accepted);
    writer_fence::close_fence(&f.config, &token).unwrap();
    f.fence();
    assert!(recovery::evidence_check(&f.config, Some(f.now)).is_err());
    f.config["recovery"]["require_writer_fence"] = json!(false);
    recovery::evidence_check(&f.config, Some(f.now)).unwrap();
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
fn record_queries_use_the_recovery_verification_bound_in_both_engines() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    executable(&f.p("package/bin/psql"), "sleep 2\nprintf 'rows\\n'");
    let socket = Path::new("/local/socket");
    let mut short = f.config.clone();
    short["recovery"]["verify_timeout_seconds"] = json!(0.5);
    assert!(recovery::record_query_leased(&short, socket, 5432, "app", "SELECT 1", &[]).is_err());
    // Auxiliary (compatibility/inventory) queries keep the generic bound.
    assert_eq!(
        recovery::query(&short, socket, 5432, "app", "SELECT 1").unwrap(),
        "rows\n"
    );
    let mut long = f.config.clone();
    long["recovery"]["verify_timeout_seconds"] = json!(30);
    assert_eq!(
        recovery::record_query_leased(&long, socket, 5432, "app", "SELECT 1", &[]).unwrap(),
        "rows\n"
    );
    let script = r#"import json,subprocess,sys
from harbor_db import recovery
c=json.loads(sys.argv[1])
checks={'record_checks':[{'name':'app','database':'app','sql':'SELECT 1'}]}
c['recovery']['verify_timeout_seconds']=0.5
try: recovery.records(c,checks,'/local/socket',5432)
except subprocess.TimeoutExpired: pass
else: raise AssertionError('record query outlived the recovery bound')
assert recovery.query(c,'/local/socket',5432,'app','SELECT 1')=='rows\n'
c['recovery']['verify_timeout_seconds']=30
assert set(recovery.records(c,checks,'/local/socket',5432))=={'app'}
"#;
    let mut spec = harbor_db::storage::process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        f.config.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&spec).unwrap();
}

#[test]
fn preparation_captures_first_snapshot_and_reuses_exact_evidence_on_retry() {
    let _workers = WORKERS.lock().unwrap();
    let mut f = Fixture::new();
    f.seed_template();
    let prep = f.preparation();
    f.config["recovery"]["off_host_receipt_file"] = json!(f.p("backup/evidence/off-host.json"));
    // The executor's certification replaces the local drill.
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\n"
    );
    let source = fs::read(f.p("backup/evidence/records.json")).unwrap();
    assert!(f.p("backup/evidence/preparation.json").exists());
    assert!(f.prepare(&prep).is_err());
    assert_eq!(
        fs::read_to_string(f.p("preparation.log")).unwrap(),
        "readiness\nbackup\nreadiness\n"
    );
    assert_eq!(
        fs::read(f.p("backup/evidence/records.json")).unwrap(),
        source
    );
    assert!(!f.p("backup/evidence/recovery.json").exists());
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
    receipt["executor_machine_sha256"] = json!("0".repeat(64));
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
    independent["executor_machine_sha256"] = json!("0".repeat(64));
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
