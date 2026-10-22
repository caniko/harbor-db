use harbor_db::storage::{application_transition, durable, process, transition_manifest};
use serde_json::json;
use std::{fs, os::unix::fs::symlink};

#[cfg(feature = "testing")]
#[path = "fixtures/private_supervisor_root.rs"]
mod private_root;

#[cfg(feature = "testing")]
#[test]
fn unadmitted_postgres_fence_cannot_quiesce_or_change_the_plan() {
    if !private_root::enter() {
        return;
    }
    let mut f = Fixture::new();
    let primary = f.temp.path().join("primary");
    fs::create_dir(&primary).unwrap();
    let postgres = f.temp.path().join("postgres.json");
    durable::write_json(&postgres, &json!({"state_dir": primary})).unwrap();
    f.config["postgres_manifest"] = json!(postgres);
    let mut record = f.journal("planned");
    record["writer_fence_token"] = json!("a".repeat(32));
    application_transition::save(&f.config, &mut record, None).unwrap();
    let journal = application_transition::journal_path(&f.config).unwrap();
    let before = fs::read(&journal).unwrap();
    // A missing active-fence anchor must be rejected before any durable phase
    // change or startup barrier, even when the journal's token has valid syntax.
    durable::lock(&f.temp.path().join("barrier/lock"), false, true).unwrap();
    assert!(application_transition::prepare(&f.config).is_err());
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert!(!f.temp.path().join("barrier/inhibited.json").exists());
    assert!(!f.temp.path().join("systemd").exists());
}

struct Fixture {
    temp: tempfile::TempDir,
    source: serde_json::Value,
    target: serde_json::Value,
    config: serde_json::Value,
}
impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let state = root.join("authority");
        let old = root.join("old");
        let new = root.join("new");
        let barrier = root.join("barrier");
        for p in [&state, &old, &new, &barrier] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let source = json!({"resource":"demo","state_dir":state,"directories":[old],"binding":{"backend":"files"},"required_mounts":[]});
        let mut target = source.clone();
        target["directories"] = json!([new]);
        target["binding"] = json!({"backend":"target-files"});
        durable::write_json(&root.join("source.json"), &source).unwrap();
        durable::write_json(&root.join("target.json"), &target).unwrap();
        durable::write_json(
            &root.join("backup.json"),
            &json!({"root":root.join("backup-root"),"maximum_age_seconds":3600}),
        )
        .unwrap();
        let tools = root.join("bin");
        fs::create_dir(&tools).unwrap();
        let exe = std::env::current_exe().unwrap();
        symlink(&exe, tools.join("harbor-db-application-backup")).unwrap();
        let command =
            json!({"user":harbor_db::storage::login_shell::current_user().unwrap(),"argv":[exe]});
        let config = json!({"version":1,"resource":"demo","source_manifest":root.join("source.json"),"target_manifest":root.join("target.json"),"barrier_dir":barrier,"drop_in_root":root.join("systemd"),"systemctl":exe,"busctl":exe,"units":["demo.service"],"retired_units":[],"timeout_seconds":10,"commands":{"import":command,"verify-target":command,"verify-source":command,"health":command},"executable_files":[],"postgres_manifest":null,"postgres_socket":"/run/postgresql","postgres_port":5432,"custody_manifest":null,"backup_manifest":root.join("backup.json"),"independent_receipt":root.join("independent.json"),"storage_package":tools,"runuser":exe});
        Self {
            temp,
            source,
            target,
            config,
        }
    }
    fn journal(&self, phase: &str) -> serde_json::Value {
        let record = json!({"version":1,"phase":phase,"intent":transition_manifest::intent(&self.config).unwrap(),"candidate":"/nix/store/00000000000000000000000000000000-generation","source_generation":"/nix/store/00000000000000000000000000000000-source","writer_fence_token":null,"fence":null});
        durable::write_json(
            &application_transition::journal_path(&self.config).unwrap(),
            &record,
        )
        .unwrap();
        record
    }
}

fn python_transition_validation(config: &serde_json::Value) -> String {
    // Model the Nix builder's fixture-owned paths as root-owned, as in the
    // other transition tests; keep all path and mode admission checks active.
    let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import transition_manifest
root_uid=int(sys.argv[2])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info); fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
try: transition_manifest.validate(json.loads(sys.argv[1]))
except (ValueError,RuntimeError,OSError) as error: print(str(error))
else: print('ACCEPTED')
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    String::from_utf8(process::execute(&spec).unwrap()).unwrap()
}

#[test]
fn transition_capture_honors_the_nested_backup_stage_budgets_in_both_engines() {
    transition_capture_fixture(false, false);
}

#[test]
fn external_backup_manifest_cannot_share_a_transition_barrier_in_both_engines() {
    let f = Fixture::new();
    let barrier = f.temp.path().join("barrier");
    for backup in [
        barrier.clone(),
        barrier.join("backup"),
        f.temp.path().to_path_buf(),
    ] {
        durable::write_json(
            &f.temp.path().join("backup.json"),
            &json!({"root":backup,"maximum_age_seconds":3600}),
        )
        .unwrap();
        let error = transition_manifest::validate(&f.config).unwrap_err();
        assert!(
            error.to_string().contains("backup and transition barrier"),
            "{error}"
        );
        assert!(python_transition_validation(&f.config).contains("backup and transition barrier"));
        assert!(!barrier.join("lock").exists());
        assert!(!barrier.join("inhibited.json").exists());
        assert!(!f.temp.path().join("authority/transition.json").exists());
    }
    durable::write_json(
        &f.temp.path().join("backup.json"),
        &json!({"root":f.temp.path().join("barrier-sibling"),"maximum_age_seconds":3600}),
    )
    .unwrap();
    transition_manifest::validate(&f.config).unwrap();
    assert_eq!(python_transition_validation(&f.config).trim(), "ACCEPTED");
}

#[test]
fn independent_receipts_require_protected_regular_inodes_and_ancestors_in_both_engines() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let path = f.temp.path().join("independent.json");
    durable::write_json(&path, &json!({"status":"verified"})).unwrap();
    let accepted = transition_manifest::independent_receipt(&f.config).unwrap();
    assert_eq!(
        accepted.1,
        harbor_db::storage::codec::file_digest(&path).unwrap()
    );
    for (selected, mode) in [(path.as_path(), 0o620), (f.temp.path(), 0o770)] {
        let previous = fs::metadata(selected).unwrap().permissions();
        fs::set_permissions(selected, fs::Permissions::from_mode(mode)).unwrap();
        assert!(transition_manifest::independent_receipt(&f.config).is_err());
        assert!(python_transition_validation(&f.config).contains("storage"));
        fs::set_permissions(selected, previous).unwrap();
    }
    let alias = f.temp.path().join("receipt-alias");
    fs::hard_link(&path, &alias).unwrap();
    assert!(transition_manifest::independent_receipt(&f.config).is_err());
    assert!(python_transition_validation(&f.config).contains("protected coordinator-owned"));
    fs::remove_file(&alias).unwrap();
    fs::rename(&path, &alias).unwrap();
    symlink(&alias, &path).unwrap();
    assert!(transition_manifest::independent_receipt(&f.config).is_err());
    assert!(python_transition_validation(&f.config).contains("redirected"));
    assert!(!f.temp.path().join("authority/transition.json").exists());
    assert!(!f.temp.path().join("barrier/lock").exists());
}

#[test]
fn independent_receipts_cannot_alias_transition_control_storage_in_both_engines() {
    let mut f = Fixture::new();
    for suffix in [
        "authority/identity.json",
        "authority/transition.json",
        "authority/lock",
        "authority/custody.json",
        "authority/independent.json",
        "authority/receipts/independent.json",
        "barrier/inhibited.json",
        "barrier/retired/inhibited.json",
        "barrier/lock",
        "systemd/demo.service.d/zzzz-harbor-db-backend-transition.conf",
    ] {
        f.config["independent_receipt"] = json!(f.temp.path().join(suffix));
        let error = transition_manifest::validate(&f.config).unwrap_err();
        assert!(
            error.to_string().contains("receipt and transition control"),
            "{suffix}: {error}"
        );
        assert!(python_transition_validation(&f.config).contains("receipt and transition control"));
        assert!(!f.temp.path().join("barrier/inhibited.json").exists());
        assert!(!f.temp.path().join("authority/transition.json").exists());
        assert!(!f.temp.path().join("barrier/lock").exists());
    }
    f.config["independent_receipt"] =
        json!(f.temp.path().join("authority-sibling/independent.json"));
    transition_manifest::validate(&f.config).unwrap();
    assert_eq!(python_transition_validation(&f.config).trim(), "ACCEPTED");
    let custody = f.temp.path().join("authority/custom-custody.json");
    let selected = f.temp.path().join("custody-manifest.json");
    durable::write_json(
        &selected,
        &json!({"kind":"filesystem","authority":f.target,"custody_file":custody}),
    )
    .unwrap();
    f.config["custody_manifest"] = json!(selected);
    f.config["independent_receipt"] = json!(custody);
    assert!(
        transition_manifest::validate(&f.config)
            .unwrap_err()
            .to_string()
            .contains("receipt and transition control")
    );
    assert!(python_transition_validation(&f.config).contains("receipt and transition control"));
    f.config["independent_receipt"] = json!(f.temp.path().join("authority/independent.json"));
    assert!(transition_manifest::validate(&f.config).is_err());
    assert!(python_transition_validation(&f.config).contains("receipt and transition control"));
}

#[test]
fn independent_receipts_cannot_alias_backup_publication_storage_before_inhibition_in_both_engines()
{
    let mut f = Fixture::new();
    for suffix in [
        "backup-root",
        "backup-root/lock",
        "backup-root/LAST_SUCCESS",
        "backup-root/point/acceptance.json",
        "backup-root/point.partial/capture.json",
        "backup-root/point.intent.json",
        "backup-root/point.credentials/token",
        "backup-root/point.pruning/acceptance.json",
    ] {
        f.config["independent_receipt"] = json!(f.temp.path().join(suffix));
        assert!(
            transition_manifest::validate(&f.config)
                .unwrap_err()
                .to_string()
                .contains("receipt and backup publication")
        );
        assert!(python_transition_validation(&f.config).contains("receipt and backup publication"));
        assert!(!f.temp.path().join("barrier/inhibited.json").exists());
        assert!(!f.temp.path().join("authority/transition.json").exists());
        assert!(!f.temp.path().join("barrier/lock").exists());
    }
    f.config["independent_receipt"] =
        json!(f.temp.path().join("backup-root-sibling/independent.json"));
    transition_manifest::validate(&f.config).unwrap();
    assert_eq!(python_transition_validation(&f.config).trim(), "ACCEPTED");
}

#[test]
fn independent_receipts_cannot_alias_transition_corpora_before_inhibition_in_both_engines() {
    let mut f = Fixture::new();
    let source = f.temp.path().join("old");
    let target = f.temp.path().join("new");
    for root in [&source, &target] {
        fs::write(root.join("records"), "retained records").unwrap();
    }
    for receipt in [
        source.clone(),
        source.join("receipt.json"),
        target.clone(),
        target.join("receipt.json"),
    ] {
        f.config["independent_receipt"] = json!(receipt);
        assert!(
            transition_manifest::validate(&f.config)
                .unwrap_err()
                .to_string()
                .contains("receipt and corpus")
        );
        assert!(python_transition_validation(&f.config).contains("receipt and corpus"));
        assert!(!f.temp.path().join("barrier/lock").exists());
        assert!(!f.temp.path().join("barrier/inhibited.json").exists());
        assert!(!f.temp.path().join("authority/transition.json").exists());
        for root in [&source, &target] {
            assert_eq!(
                fs::read_to_string(root.join("records")).unwrap(),
                "retained records"
            );
        }
    }
    // A source/corpus ancestor is rejected before backup-overlap admission too.
    f.config["independent_receipt"] = json!(f.temp.path());
    assert!(transition_manifest::validate(&f.config).is_err());
    assert_ne!(python_transition_validation(&f.config).trim(), "ACCEPTED");
    for receipt in [
        f.temp.path().join("old-sibling/receipt.json"),
        f.temp.path().join("new-sibling/receipt.json"),
    ] {
        f.config["independent_receipt"] = json!(receipt);
        transition_manifest::validate(&f.config).unwrap();
        assert_eq!(python_transition_validation(&f.config).trim(), "ACCEPTED");
    }
}
#[test]
fn repeated_transition_runs_capture_intervening_writes_and_retry_only_their_own_attempt_in_both_engines()
 {
    transition_capture_fixture(false, true);
}

#[test]
fn transition_backups_cannot_overlap_the_shared_authority_in_both_engines() {
    let mut f = Fixture::new();
    let authority = f.temp.path().join("authority/child");
    fs::create_dir(&authority).unwrap();
    f.source["state_dir"] = json!(authority);
    f.target["state_dir"] = json!(authority);
    durable::write_json(&f.temp.path().join("source.json"), &f.source).unwrap();
    durable::write_json(&f.temp.path().join("target.json"), &f.target).unwrap();
    for backup in [
        authority.clone(),
        authority.join("backup"),
        f.temp.path().join("authority"),
    ] {
        durable::write_json(
            &f.temp.path().join("backup.json"),
            &json!({"root":backup,"maximum_age_seconds":3600}),
        )
        .unwrap();
        let error = transition_manifest::validate(&f.config).unwrap_err();
        assert!(
            error.to_string().contains("backup and authority roots"),
            "{error}"
        );
        assert!(python_transition_validation(&f.config).contains("backup and authority roots"));
        assert!(!authority.join("lock").exists());
        assert!(!authority.join("transition.json").exists());
        assert!(!f.temp.path().join("barrier/inhibited.json").exists());
    }
    durable::write_json(
        &f.temp.path().join("backup.json"),
        &json!({"root":f.temp.path().join("authority-sibling"),"maximum_age_seconds":3600}),
    )
    .unwrap();
    transition_manifest::validate(&f.config).unwrap();
    assert_eq!(python_transition_validation(&f.config).trim(), "ACCEPTED");
}

#[test]
fn barrier_and_guarded_storage_must_be_disjoint_in_both_directions_in_both_engines() {
    let mut f = Fixture::new();
    let barrier = f.temp.path().join("barrier");
    for source in [false, true] {
        for directory in [
            barrier.clone(),
            barrier.join("data"),
            f.temp.path().to_path_buf(),
        ] {
            let mut policy = if source {
                f.source.clone()
            } else {
                f.target.clone()
            };
            policy["directories"] = json!([directory]);
            let path = f
                .temp
                .path()
                .join(if source { "source.json" } else { "target.json" });
            durable::write_json(&path, &policy).unwrap();
            let error = transition_manifest::validate(&f.config).unwrap_err();
            assert!(error.to_string().contains("guarded storage"), "{error}");
            let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import transition_manifest
root_uid=int(sys.argv[2])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info); fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
try: transition_manifest.validate(json.loads(sys.argv[1]))
except ValueError as error: assert 'guarded storage' in str(error)
else: raise AssertionError('overlapping guarded storage accepted')
"#;
            let mut spec = process::CommandSpec::new(vec![
                "python3".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                f.config.to_string(),
                option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
            ]);
            spec.environment = Some(std::collections::BTreeMap::from([
                (
                    "PYTHONPATH".into(),
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                ),
                ("PATH".into(), std::env::var("PATH").unwrap()),
            ]));
            process::execute(&spec).unwrap();
            durable::write_json(&path, if source { &f.source } else { &f.target }).unwrap();
        }
    }
    f.config["barrier_dir"] = json!(f.temp.path().join("old-sibling"));
    transition_manifest::validate(&f.config).unwrap();
    assert!(!f.temp.path().join("old-sibling").exists());
    assert!(!f.temp.path().join("authority/transition.json").exists());
}
#[test]
fn transition_source_and_target_roots_must_be_disjoint_before_any_writes_in_both_engines() {
    let f = Fixture::new();
    let source = f.temp.path().join("old");
    for directory in [source.clone(), source.join("nested")] {
        let mut target = f.target.clone();
        target["directories"] = json!([directory]);
        durable::write_json(&f.temp.path().join("target.json"), &target).unwrap();
        let native = transition_manifest::validate(&f.config);
        let peer = python_transition_validation(&f.config);
        assert!(native.is_err(), "overlapping import target accepted");
        assert!(
            native
                .unwrap_err()
                .to_string()
                .contains("source and target corpus roots must be disjoint")
        );
        assert!(
            peer.contains("source and target corpus roots must be disjoint"),
            "{peer}"
        );
        assert!(!f.temp.path().join("authority/transition.json").exists());
        assert!(!f.temp.path().join("barrier/inhibited.json").exists());
    }
    let mut nested_source = f.source.clone();
    nested_source["directories"] = json!([source.join("nested")]);
    durable::write_json(&f.temp.path().join("source.json"), &nested_source).unwrap();
    let mut parent_target = f.target.clone();
    parent_target["directories"] = json!([source]);
    durable::write_json(&f.temp.path().join("target.json"), &parent_target).unwrap();
    assert!(
        transition_manifest::validate(&f.config)
            .unwrap_err()
            .to_string()
            .contains("source and target corpus roots must be disjoint")
    );
    durable::write_json(&f.temp.path().join("source.json"), &f.source).unwrap();
    durable::write_json(&f.temp.path().join("target.json"), &f.target).unwrap();
    transition_manifest::validate(&f.config).unwrap();
}

#[test]
fn transition_backup_root_must_be_disjoint_from_both_corpora_in_both_engines() {
    let f = Fixture::new();
    for corpus in ["old", "new"] {
        for directory in [
            f.temp.path().join(corpus),
            f.temp.path().join(corpus).join("backup"),
        ] {
            durable::write_json(
                &f.temp.path().join("backup.json"),
                &json!({"root":directory,"maximum_age_seconds":3600}),
            )
            .unwrap();
            let peer = python_transition_validation(&f.config);
            let error = transition_manifest::validate(&f.config).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("backup and corpus roots must be disjoint"),
                "{error}"
            );
            assert!(
                peer.contains("backup and corpus roots must be disjoint"),
                "{peer}"
            );
            assert!(!f.temp.path().join("authority/transition.json").exists());
            assert!(!f.temp.path().join("barrier/inhibited.json").exists());
        }
    }
    // A backup ancestor overlaps a nested corpus, even with no shared text prefix
    // between the source and target themselves.
    let mut source = f.source.clone();
    source["directories"] = json!([f.temp.path().join("old/nested")]);
    durable::write_json(&f.temp.path().join("source.json"), &source).unwrap();
    durable::write_json(
        &f.temp.path().join("backup.json"),
        &json!({"root":f.temp.path().join("old"),"maximum_age_seconds":3600}),
    )
    .unwrap();
    assert!(transition_manifest::validate(&f.config).is_err());
    durable::write_json(
        &f.temp.path().join("backup.json"),
        &json!({"root":f.temp.path().join("old-sibling"),"maximum_age_seconds":3600}),
    )
    .unwrap();
    transition_manifest::validate(&f.config).unwrap();
}

#[test]
fn transition_capture_uses_the_backup_owner_instead_of_the_authority_owner_in_both_engines() {
    transition_capture_fixture(true, false);
}

fn transition_capture_fixture(different_authority: bool, repeat: bool) {
    use std::collections::BTreeMap;
    for python in [false, true] {
        let mut f = Fixture::new();
        if different_authority {
            let immutable = env!("HARBOR_DB_TEST_CONFIG_FIXTURE");
            assert_ne!(
                std::os::unix::fs::MetadataExt::uid(&fs::metadata(immutable).unwrap()),
                unsafe { libc::geteuid() }
            );
            f.source["state_dir"] = json!(immutable);
            f.target["state_dir"] = json!(immutable);
            durable::write_json(&f.temp.path().join("source.json"), &f.source).unwrap();
            durable::write_json(&f.temp.path().join("target.json"), &f.target).unwrap();
        } else {
            f.config["timeout_seconds"] = json!(1);
        }
        let root = f.temp.path().join("backup-root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("lock"), b"").unwrap();
        let adapter = f.temp.path().join("adapter.py");
        fs::write(f.temp.path().join("source-record"), b"stable source").unwrap();
        fs::write(&adapter, r#"import hashlib,json,sys,time
from pathlib import Path
stage,backup,workspace=sys.argv[1],Path(sys.argv[2]),Path(sys.argv[3])
base=Path(__file__).parent
if not (base/'repeat').exists(): time.sleep(0.6)
if stage=='capture':
    backup.mkdir(); (backup/'records').write_bytes((base/'source-record').read_bytes())
    semantic=hashlib.sha256((backup/'records').read_bytes()).hexdigest()
    (backup/'capture.json').write_text(json.dumps({'version':1,'semantic_sha256':semantic,'consistency':'quiesced'}))
elif stage=='cleanup': pass
elif stage=='restore': (workspace/'records').write_bytes((backup/'records').read_bytes())
elif stage=='verify': print(json.dumps({'version':1,'status':'verified','semantic_sha256':hashlib.sha256((workspace/'records').read_bytes()).hexdigest()}))
"#).unwrap();
        let interpreter = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("python3"))
            .find(|path| path.is_file())
            .unwrap();
        let commands: serde_json::Map<String, serde_json::Value> =
            ["capture", "restore", "verify", "cleanup"]
                .into_iter()
                .map(|stage| {
                    (
                        stage.into(),
                        json!([interpreter, adapter, stage, "{backup}", "{workspace}"]),
                    )
                })
                .collect();
        let backup = json!({"version":1,"resource":"demo","root":root,"commands":commands,"executable_files":[adapter],"timeout_seconds":10,"maximum_age_seconds":3600});
        durable::write_json(&f.temp.path().join("backup.json"), &backup).unwrap();
        let binary = f.temp.path().join("bin/harbor-db-application-backup");
        fs::remove_file(&binary).unwrap();
        symlink(env!("CARGO_BIN_EXE_harbor-db-application-backup"), &binary).unwrap();
        if repeat {
            fs::write(f.temp.path().join("repeat"), b"").unwrap();
        }
        let record = json!({"intent":transition_manifest::intent(&f.config).unwrap(),"run_id":"a".repeat(32)});
        let capture = |record: &serde_json::Value| {
            if python {
                let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_backup,application_transition
root_uid=int(sys.argv[4])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info); fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
original_digest=application_backup.digest
if sys.argv[3]: patch.object(application_backup,'machine',side_effect=lambda: original_digest(sys.argv[3])).start()
backup,receipt=application_transition.capture_source(json.loads(sys.argv[1]),json.loads(sys.argv[2]),())
assert receipt['consistency']=='quiesced' and (backup/'acceptance.json').is_file()
print(json.dumps({'backup':str(backup),'receipt':receipt}))
"#;
                let mut spec = process::CommandSpec::new(vec![
                    interpreter.display().to_string(),
                    "-B".into(),
                    "-c".into(),
                    script.into(),
                    f.config.to_string(),
                    record.to_string(),
                    option_env!("HARBOR_DB_TEST_MACHINE_ID")
                        .unwrap_or("")
                        .into(),
                    option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
                ]);
                spec.environment = Some(BTreeMap::from([
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                    ("TMPDIR".into(), std::env::var("TMPDIR").unwrap()),
                ]));
                let outcome: serde_json::Value =
                    serde_json::from_slice(&process::execute(&spec).unwrap()).unwrap();
                (
                    std::path::PathBuf::from(outcome["backup"].as_str().unwrap()),
                    outcome["receipt"].clone(),
                )
            } else {
                let (path, receipt) =
                    application_transition::capture_source(&f.config, record, &[]).unwrap();
                assert_eq!(receipt["consistency"], "quiesced");
                assert!(path.join("acceptance.json").is_file());
                (path, receipt)
            }
        };
        let (first, accepted) = capture(&record);
        if repeat {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let acceptance = first.join("acceptance.json");
            let original = fs::read(&acceptance).unwrap();
            let inode = fs::metadata(&acceptance).unwrap().ino();
            fs::write(
                f.temp.path().join("source-record"),
                b"intervening acknowledged write",
            )
            .unwrap();
            assert_eq!(
                capture(&record),
                (first.clone(), accepted.clone()),
                "same-run retries must retain their original capture"
            );
            assert_eq!(fs::read(&acceptance).unwrap(), original);
            assert_eq!(fs::metadata(&acceptance).unwrap().ino(), inode);
            let mut second_record = record.clone();
            second_record["run_id"] = json!("b".repeat(32));
            let (second, new) = capture(&second_record);
            assert_ne!(second, first, "a new run reused a previous capture");
            assert_eq!(
                fs::read(second.join("records")).unwrap(),
                b"intervening acknowledged write"
            );
            assert_ne!(new["semantic_sha256"], accepted["semantic_sha256"]);
            let mut stale = accepted;
            stale["captured_at"] = json!(0);
            fs::set_permissions(&acceptance, fs::Permissions::from_mode(0o640)).unwrap();
            durable::write_json(&acceptance, &stale).unwrap();
            fs::write(
                f.temp.path().join("source-record"),
                b"latest acknowledged write",
            )
            .unwrap();
            let mut third_record = record.clone();
            third_record["run_id"] = json!("c".repeat(32));
            let (third, latest) = capture(&third_record);
            assert_ne!(third, second);
            assert_eq!(
                fs::read(third.join("records")).unwrap(),
                b"latest acknowledged write"
            );
            assert_ne!(latest["semantic_sha256"], new["semantic_sha256"]);
        }
        assert!(root.join("LAST_SUCCESS").is_file());
    }
}

#[test]
fn operator_policy_requires_every_declared_manifest_to_be_immutable() {
    use std::os::unix::fs::PermissionsExt;
    let Some(fixture) = option_env!("HARBOR_DB_TEST_CONFIG_FIXTURE") else {
        panic!("operator qualification requires the immutable configuration fixture");
    };
    let fixture = std::path::Path::new(fixture);
    let stored_alias = fixture.join("postgresql.json");
    let mut config = json!({
        "source_manifest":stored_alias,"target_manifest":stored_alias,"backup_manifest":stored_alias,
        "postgres_manifest":null,"custody_manifest":null
    });
    transition_manifest::validate_operator_policy(&config).unwrap();
    let root = tempfile::tempdir().unwrap();
    let mutable = root.path().join("policy.json");
    fs::write(&mutable, b"{}").unwrap();
    fs::set_permissions(&mutable, fs::Permissions::from_mode(0o444)).unwrap();
    let runtime_alias = root.path().join("runtime-alias.json");
    symlink(&stored_alias, &runtime_alias).unwrap();
    for key in [
        "postgres_manifest",
        "custody_manifest",
        "source_manifest",
        "target_manifest",
        "backup_manifest",
    ] {
        let previous = config[key].clone();
        for path in [&mutable, &runtime_alias] {
            config[key] = json!(path);
            let before = config.clone();
            let error = transition_manifest::validate_operator_policy(&config).expect_err(key);
            assert!(
                error.to_string().contains("immutable store"),
                "{key}: {error}"
            );
            assert_eq!(config, before);
            assert_eq!(fs::read(&mutable).unwrap(), b"{}");
        }
        config[key] = json!(stored_alias);
        let before = config.clone();
        transition_manifest::validate_operator_policy(&config).unwrap();
        assert_eq!(
            config, before,
            "immutable aliases must preserve the recorded contract"
        );
        config[key] = previous;
    }
    let script = r#"import json,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_transition
config=json.loads(sys.argv[1])
for key in ('postgres_manifest','custody_manifest'):
    selected=config | {key:sys.argv[2]}
    with patch.object(sys,'argv',['transition','--config',sys.argv[3],'status']), \
         patch.object(application_transition,'read_json',return_value=selected), \
         patch.object(application_transition,'status') as status:
        assert application_transition.main()==1
        status.assert_not_called()
"#;
    let mut command = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        mutable.to_str().unwrap().into(),
        stored_alias.to_str().unwrap().into(),
    ]);
    command.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    process::execute(&command).unwrap();
}

#[test]
fn operator_transition_entrypoints_reject_mutable_files_and_aliases_in_both_engines() {
    let f = Fixture::new();
    let mut config = f.config.clone();
    let fixture =
        std::path::Path::new(env!("HARBOR_DB_TEST_CONFIG_FIXTURE")).join("postgresql.json");
    for key in ["source_manifest", "target_manifest", "backup_manifest"] {
        config[key] = json!(fixture);
    }
    config["postgres_manifest"] = serde_json::Value::Null;
    config["custody_manifest"] = serde_json::Value::Null;
    let executable = fs::canonicalize(
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("python3"))
            .find(|path| path.is_file())
            .unwrap(),
    )
    .unwrap();
    for stage in ["import", "verify-target", "verify-source", "health"] {
        config["commands"][stage]["argv"][0] = json!(executable);
    }
    config["executable_files"] = json!([]);
    for key in ["runuser", "systemctl", "busctl"] {
        config[key] = json!(executable);
    }
    let mutable = f.temp.path().join("worker-command");
    fs::write(&mutable, b"#!/bin/sh\nexit 0\n").unwrap();
    let alias = f.temp.path().join("worker-alias");
    symlink(&executable, &alias).unwrap();
    for stage in ["import", "verify-target", "verify-source", "health"] {
        for path in [&mutable, &alias, &fixture] {
            let mut selected = config.clone();
            selected["commands"][stage]["argv"][0] = json!(path);
            assert!(
                transition_manifest::validate_operator_policy(&selected).is_err(),
                "mutable/nonexecutable transition entrypoint admitted: {stage}"
            );
        }
    }
    transition_manifest::validate_operator_policy(&config).unwrap();
    for key in ["runuser", "systemctl", "busctl"] {
        for path in [&mutable, &alias, &fixture] {
            let mut selected = config.clone();
            selected[key] = json!(path);
            assert!(
                transition_manifest::validate_operator_policy(&selected).is_err(),
                "mutable/nonexecutable coordinator admitted: {key}"
            );
        }
    }
    let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db.transition_manifest import validate_operator_policy
config=json.loads(sys.argv[1]);mutable,alias,fixture=sys.argv[2:5];uid=int(sys.argv[5])
if uid:
    original=Path.lstat
    def mapped(path):
        info=original(path)
        if info.st_uid!=uid: return info
        fields=list(info);fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped).start()
validate_operator_policy(config)
for stage in ('import','verify-target','verify-source','health'):
    for path in (mutable,alias,fixture):
        selected=json.loads(json.dumps(config));selected['commands'][stage]['argv'][0]=path
        try: validate_operator_policy(selected)
        except (ValueError,OSError): pass
        else: raise AssertionError('mutable/nonexecutable transition worker admitted')
for key in ('runuser','systemctl','busctl'):
    for path in (mutable,alias,fixture):
        selected=json.loads(json.dumps(config));selected[key]=path
        try: validate_operator_policy(selected)
        except (ValueError,OSError): pass
        else: raise AssertionError('mutable/nonexecutable transition coordinator admitted')
"#;
    let mut command = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        mutable.display().to_string(),
        alias.display().to_string(),
        fixture.display().to_string(),
        option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
    ]);
    command.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    process::execute(&command).unwrap();
}

#[test]
fn python_and_rust_preserve_hash_sensitive_transition_intent_and_journal_phases() {
    use harbor_db::storage::process;
    let mut f = Fixture::new();
    f.config["commands"]["import"]["argv"]
        .as_array_mut()
        .unwrap()
        .push(json!("literal snowman ☃"));
    let python = |write: bool, record: &serde_json::Value| {
        let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_transition,transition_manifest
root_uid=int(sys.argv[4])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info);fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
config,write,record=json.loads(sys.argv[1]),sys.argv[2]=='true',json.loads(sys.argv[3])
assert record['intent']==transition_manifest.intent(config)
if write: application_transition.save(config,record)
print(json.dumps(application_transition.status(config)))
"#;
        let mut command = process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            script.into(),
            f.config.to_string(),
            write.to_string(),
            record.to_string(),
            option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
        ]);
        command.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        harbor_db::storage::codec::decode(&process::execute(&command).unwrap()).unwrap()
    };
    for phase in [
        "planned",
        "quiescing",
        "quiesced",
        "captured",
        "importing",
        "imported",
        "prepared",
        "committing",
        "committed",
        "write-enabled",
        "complete",
        "aborting",
        "aborted",
    ] {
        let mut record = f.journal(phase);
        record["retained_obligation"] = json!({
            "unknown_future_key":"preserved ☃", "generation":42,
            "literal_number_object": {"$serde_json::private::Number": "123"},
            "literal_raw_object": {"$serde_json::private::RawValue": "[1,2]"}
        });
        let path = application_transition::journal_path(&f.config).unwrap();
        durable::write_json(&path, &record).unwrap();
        let original = fs::read(&path).unwrap();
        assert_eq!(
            python(false, &record),
            record,
            "Python must read native phase {phase}"
        );
        assert_eq!(
            python(true, &record),
            record,
            "Python save must preserve native intent and unknown obligations"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "hash-sensitive journal bytes changed for {phase}"
        );
        assert_eq!(
            application_transition::status(&f.config).unwrap(),
            record,
            "Rust must read Python phase {phase}"
        );
    }
}

#[test]
fn manifest_validation_preserves_exact_key_and_endpoint_contracts() {
    let f = Fixture::new();
    transition_manifest::validate(&f.config).unwrap();
    let mut changed = f.config.clone();
    changed["extra"] = json!(true);
    assert!(transition_manifest::validate(&changed).is_err());
    for (key, value) in [
        ("timeout_seconds", json!(false)),
        ("timeout_seconds", json!(86401)),
        ("postgres_port", json!(false)),
        ("postgres_port", json!(65536)),
        ("postgres_socket", json!("relative")),
        ("units", json!(["postgresql.service"])),
        ("units", json!(["demo.service", "demo.service"])),
        ("retired_units", json!(["demo.service"])),
        ("units", json!(["bad;unit.service"])),
        ("barrier_dir", f.source["state_dir"].clone()),
        ("systemctl", json!("/unsafe%path")),
    ] {
        let mut changed = f.config.clone();
        changed[key] = value;
        assert!(transition_manifest::validate(&changed).is_err(), "{key}");
    }
}
#[test]
fn source_target_must_share_authority_and_change_binding() {
    let f = Fixture::new();
    let path = f.temp.path().join("target.json");
    durable::write_json(&path, &f.source).unwrap();
    assert!(transition_manifest::validate(&f.config).is_err());
    let mut target = f.target.clone();
    target["state_dir"] = json!(f.temp.path().join("other"));
    durable::write_json(&path, &target).unwrap();
    assert!(transition_manifest::validate(&f.config).is_err());
    target = f.target.clone();
    target["binding"]["backend"] = json!("postgresql");
    durable::write_json(&path, &target).unwrap();
    assert!(
        transition_manifest::validate(&f.config)
            .unwrap_err()
            .to_string()
            .contains("writer fence")
    );
}
#[test]
fn workers_require_declared_account_absolute_argv_and_exact_shape() {
    let f = Fixture::new();
    for command in [
        json!({"user":"root","argv":[]}),
        json!({"user":"root","argv":["relative"]}),
        json!({"user":"root","argv":[false]}),
        json!({"user":"no-such-harbor-account","argv":["/not-used"]}),
        json!({"user":"root","argv":["/not-used"],"extra":true}),
    ] {
        let mut config = f.config.clone();
        config["commands"]["import"] = command;
        assert!(transition_manifest::validate(&config).is_err());
    }
    let mut config = f.config.clone();
    config["commands"]
        .as_object_mut()
        .unwrap()
        .remove("verify-source");
    assert!(transition_manifest::validate(&config).is_err());
}
#[test]
fn legacy_journal_resume_binds_manifest_and_executable_bytes() {
    let f = Fixture::new();
    let record = f.journal("captured");
    assert_eq!(application_transition::status(&f.config).unwrap(), record);
    let mut changed = f.config.clone();
    changed["commands"]["import"]["argv"]
        .as_array_mut()
        .unwrap()
        .push(json!("changed"));
    assert!(
        application_transition::status(&changed)
            .unwrap_err()
            .to_string()
            .contains("identity changed")
    );
    let path = f.temp.path().join("adapter");
    fs::write(&path, b"original adapter").unwrap();
    let mut config = f.config.clone();
    config["executable_files"] = json!([path]);
    let mut record = record;
    record["intent"] = transition_manifest::intent(&config).unwrap();
    durable::write_json(
        &application_transition::journal_path(&config).unwrap(),
        &record,
    )
    .unwrap();
    fs::write(&path, b"changed adapter").unwrap();
    assert!(application_transition::status(&config).is_err());
}
#[test]
fn changed_source_manifest_and_journal_version_reject_resume() {
    let f = Fixture::new();
    let mut record = f.journal("planned");
    record["version"] = json!(2);
    durable::write_json(
        &application_transition::journal_path(&f.config).unwrap(),
        &record,
    )
    .unwrap();
    assert!(application_transition::status(&f.config).is_err());
    f.journal("planned");
    let mut source = f.source.clone();
    source["required_files"] = json!([]);
    durable::write_json(&f.temp.path().join("source.json"), &source).unwrap();
    assert!(application_transition::status(&f.config).is_err());
}
#[test]
fn prepared_admission_rejects_startup_wrong_authority_and_unbound_generation() {
    let f = Fixture::new();
    let mut record = f.journal("prepared");
    record["candidate"] = serde_json::Value::Null;
    durable::write_json(
        &application_transition::journal_path(&f.config).unwrap(),
        &record,
    )
    .unwrap();
    assert!(
        application_transition::admission(&f.config, "startup", &f.target, None)
            .unwrap_err()
            .to_string()
            .contains("ordinary startup")
    );
    assert!(
        application_transition::admission(&f.config, "preflight", &f.source, None)
            .unwrap_err()
            .to_string()
            .contains("target contract")
    );
    assert!(
        application_transition::admission(&f.config, "activate", &f.target, None)
            .unwrap_err()
            .to_string()
            .contains("bound realized")
    );
    record["candidate"] = json!("generation");
    durable::write_json(
        &application_transition::journal_path(&f.config).unwrap(),
        &record,
    )
    .unwrap();
    assert!(
        application_transition::admission(&f.config, "activate", &f.target, Some("other"))
            .unwrap_err()
            .to_string()
            .contains("activation generation differs")
    );
}
#[test]
fn unfinished_journal_inhibits_ordinary_resource_startup() {
    let f = Fixture::new();
    for phase in [
        "quiescing",
        "quiesced",
        "captured",
        "importing",
        "imported",
        "prepared",
        "committing",
        "committed",
        "aborting",
    ] {
        f.journal(phase);
        assert!(
            harbor_db::storage::resource::require_stable(&f.source).is_err(),
            "{phase}"
        );
    }
    for phase in ["planned", "write-enabled", "complete", "aborted"] {
        f.journal(phase);
        harbor_db::storage::resource::require_stable(&f.source).unwrap();
    }
}
#[test]
fn publication_is_compare_and_swap_and_preserves_identity() {
    let f = Fixture::new();
    harbor_db::storage::resource::adopt(&f.source, "retained-resource").unwrap();
    let old = harbor_db::storage::resource::verify(
        &f.source,
        &harbor_db::storage::resource::contract(&f.source).unwrap(),
    )
    .unwrap();
    let mut expected = harbor_db::storage::resource::contract(&f.target).unwrap();
    expected["identity"] = json!("retained-resource");
    application_transition::publish(&f.config, &f.target, &expected, &old).unwrap();
    application_transition::publish(&f.config, &f.target, &expected, &old).unwrap();
    assert_eq!(
        harbor_db::storage::resource::verify(&f.target, &expected).unwrap(),
        expected
    );
    assert!(harbor_db::storage::resource::verify(&f.source, &old).is_err());
    application_transition::publish(&f.config, &f.source, &old, &expected).unwrap();
    harbor_db::storage::resource::verify(&f.source, &old).unwrap();
    let state = std::path::Path::new(f.source["state_dir"].as_str().unwrap());
    let mut foreign = old.clone();
    foreign["identity"] = json!("foreign");
    durable::write_json(&state.join("identity.json"), &foreign).unwrap();
    assert!(application_transition::publish(&f.config, &f.target, &expected, &old).is_err());
    assert_eq!(
        durable::read_json(&state.join("identity.json")).unwrap(),
        foreign
    );
}
#[test]
fn publication_rejects_foreign_root_marker() {
    let f = Fixture::new();
    harbor_db::storage::resource::adopt(&f.source, "retained").unwrap();
    let old = harbor_db::storage::resource::verify(
        &f.source,
        &harbor_db::storage::resource::contract(&f.source).unwrap(),
    )
    .unwrap();
    let mut expected = harbor_db::storage::resource::contract(&f.target).unwrap();
    expected["identity"] = json!("retained");
    let marker = std::path::Path::new(f.target["directories"][0].as_str().unwrap())
        .join(".harbor-db-demo-identity.json");
    durable::write_json(&marker, &json!({"resource":"demo","identity":"foreign"})).unwrap();
    assert!(application_transition::publish(&f.config, &f.target, &expected, &old).is_err());
    harbor_db::storage::resource::verify(&f.source, &old).unwrap();
}
#[test]
fn release_keeps_retired_writers_inhibited_and_checks_marker_ownership() {
    let f = Fixture::new();
    let mut config = f.config.clone();
    config["retired_units"] = json!(["legacy.service"]);
    let record = json!({"intent":{"test":true},"barrier_candidate":null,"candidate":"accepted-generation","source_generation":"retained-generation"});
    let barrier = std::path::Path::new(config["barrier_dir"].as_str().unwrap());
    fs::create_dir(barrier.join("retired")).unwrap();
    durable::write_json(
        &barrier.join("inhibited.json"),
        &json!({"intent":record["intent"],"candidate":null}),
    )
    .unwrap();
    let retired = json!({"resource":"demo","units":["legacy.service"]});
    durable::write_json(&barrier.join("retired/inhibited.json"), &retired).unwrap();
    transition_manifest::release_barriers(&config, &record, false).unwrap();
    assert!(!barrier.join("inhibited.json").exists());
    assert!(barrier.join("retired/inhibited.json").exists());
    assert_eq!(
        durable::read_json(&barrier.join("start-policy.json")).unwrap()["units"],
        json!(["demo.service"])
    );
    transition_manifest::release_barriers(&config, &record, true).unwrap();
    assert!(!barrier.join("retired/inhibited.json").exists());
    let policy = durable::read_json(&barrier.join("start-policy.json")).unwrap();
    assert_eq!(policy["generation"], "retained-generation");
    assert_eq!(policy["units"], json!(["demo.service", "legacy.service"]));
    durable::write_json(
        &barrier.join("inhibited.json"),
        &json!({"intent":"foreign"}),
    )
    .unwrap();
    assert!(transition_manifest::release_barriers(&config, &record, false).is_err());
    assert!(barrier.join("inhibited.json").exists());
}
#[test]
fn persistent_drop_in_is_exact_and_foreign_policy_preserved() {
    let f = Fixture::new();
    let selected = f.temp.path().join("barrier");
    let path = f.temp.path().join("policy.conf");
    let content =
        transition_manifest::transition_content(&f.config, &selected, "demo.service").unwrap();
    assert!(String::from_utf8_lossy(&content).contains("ExecCondition=+"));
    durable::atomic_write(&path, &content).unwrap();
    transition_manifest::check_transition_drop_in(&f.config, &path, &selected, "demo.service")
        .unwrap();
    fs::write(&path, b"foreign policy").unwrap();
    assert!(
        transition_manifest::check_transition_drop_in(&f.config, &path, &selected, "demo.service")
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), b"foreign policy");
}
#[test]
fn candidate_rejects_mutable_missing_and_unicode_hashes_without_panics() {
    let f = Fixture::new();
    for candidate in [
        "/run/current-system",
        "/nix/store/not-a-contract",
        "/nix/store/00000000000000000000000000000000-absent",
        "/nix/store/0000000000000000000000000000000é-invalid",
    ] {
        assert!(transition_manifest::candidate_path(candidate, &f.config).is_err());
    }
}
#[test]
fn preparation_worker_cannot_use_an_unbound_candidate() {
    let f = Fixture::new();
    let mut config = f.config.clone();
    config["commands"]["import"]["argv"]
        .as_array_mut()
        .unwrap()
        .push(json!("{candidate}"));
    assert!(
        application_transition::run_action(
            &config,
            "import",
            &json!({"candidate":null,"backup":"/backup"}),
            &[]
        )
        .unwrap_err()
        .to_string()
        .contains("immutable target contract")
    );
}
#[test]
fn cli_requires_immutable_manifests_and_all_public_commands_are_declared() {
    let f = Fixture::new();
    let config = f.temp.path().join("config.json");
    durable::write_json(&config, &f.config).unwrap();
    for command in [
        "status",
        "prepare",
        "commit",
        "enable-writes",
        "complete",
        "abort",
        "retire",
    ] {
        let mut worker = std::process::Command::new(env!("CARGO_BIN_EXE_harbor-db-transition"));
        worker
            .args(["--config", config.to_str().unwrap(), command])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = process::spawn(&mut worker)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("immutable store files"));
    }
    for binary in [
        env!("CARGO_BIN_EXE_harbor-db-transition"),
        env!("CARGO_BIN_EXE_harbor-db-transition-start"),
    ] {
        let mut command = std::process::Command::new(binary);
        command.arg("--help");
        assert!(
            process::spawn(&mut command)
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
    }
}
#[test]
fn root_only_coordinator_operations_fail_before_publication() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    for result in [
        application_transition::plan(&f.config, "/run/current-system", None),
        application_transition::prepare(&f.config),
        application_transition::commit(&f.config),
        application_transition::enable_writes(&f.config),
        application_transition::complete(&f.config),
        application_transition::abort(&f.config),
        application_transition::retire(&f.config),
    ] {
        assert!(result.unwrap_err().to_string().contains("requires root"));
    }
    assert!(
        !std::path::Path::new(f.source["state_dir"].as_str().unwrap())
            .join("transition.json")
            .exists()
    );
}
#[test]
fn declared_tool_symlinks_hash_target_bytes_but_corpus_links_remain_rejected() {
    let f = Fixture::new();
    let link = std::path::Path::new(f.config["storage_package"].as_str().unwrap())
        .join("harbor-db-application-backup");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let tools = transition_manifest::tools(&f.config).unwrap();
    assert_eq!(
        tools[link.to_str().unwrap()],
        harbor_db::storage::codec::file_digest(&std::env::current_exe().unwrap()).unwrap()
    );
    assert!(harbor_db::storage::codec::file_digest(&link).is_err());
    transition_manifest::intent(&f.config).unwrap();
}
fn shell() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .expect("approved shell contains sh")
}
#[test]
fn worker_fixture_coordinator() {
    let Ok(anchor) = std::env::var("HARBOR_TEST_WORKER_ANCHOR") else {
        return;
    };
    let ready = std::env::var("HARBOR_TEST_WORKER_READY").unwrap();
    let lease = durable::lock(std::path::Path::new(&anchor), false, false).unwrap();
    let command = json!({"user":harbor_db::storage::login_shell::current_user().unwrap(),"argv":[shell(),"-c","printf '%s' \"$$\" > \"$1\"; exec sleep 30","worker",ready]});
    transition_manifest::worker(
        &json!({"timeout_seconds":60}),
        &command,
        &std::collections::BTreeMap::new(),
        &[lease.fd()],
    )
    .unwrap();
}
#[test]
fn surviving_worker_keeps_inherited_lease_after_coordinator_death() {
    let temp = tempfile::tempdir().unwrap();
    let anchor = temp.path().join("lock");
    let ready = temp.path().join("child.pid");
    drop(durable::lock(&anchor, false, true).unwrap());
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "worker_fixture_coordinator", "--nocapture"])
        .env("HARBOR_TEST_WORKER_ANCHOR", &anchor)
        .env("HARBOR_TEST_WORKER_READY", &ready)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut parent = process::spawn(&mut command).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(parent.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid: i32 = fs::read_to_string(&ready).unwrap().parse().unwrap();
    parent.kill().unwrap();
    parent.wait().unwrap();
    let blocked = durable::lock(&anchor, false, false).is_err();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if durable::lock(&anchor, false, false).is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "terminated worker retained lease"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        blocked,
        "coordinator death released a surviving worker's lease"
    );
}
#[test]
fn worker_substitutions_environment_identity_and_timeout_are_native() {
    let executable = shell();
    let user = harbor_db::storage::login_shell::current_user().unwrap();
    let mut substitutions = std::collections::BTreeMap::new();
    substitutions.insert("{target}".into(), "bound-value".into());
    let command = json!({"user":user,"argv":[executable,"-c","test \"$1\" = bound-value && test -n \"$HARBOR_DB_LEASE_FDS\" && printf '{\"version\":1,\"status\":\"verified\"}'","worker","{target}"]});
    let temp = tempfile::tempdir().unwrap();
    let lease = durable::lock(&temp.path().join("lock"), false, true).unwrap();
    let result = transition_manifest::worker(
        &json!({"timeout_seconds":5}),
        &command,
        &substitutions,
        &[lease.fd()],
    )
    .unwrap();
    assert_eq!(result, json!({"version":1,"status":"verified"}));
    let timeout = json!({"user":user,"argv":[executable,"-c","exec sleep 30"]});
    assert!(
        transition_manifest::worker(
            &json!({"timeout_seconds":1}),
            &timeout,
            &std::collections::BTreeMap::new(),
            &[lease.fd()]
        )
        .unwrap_err()
        .to_string()
        .contains("execution limit")
    );
}

fn captured_source(f: &Fixture) -> (serde_json::Value, serde_json::Value) {
    use harbor_db::storage::{application_backup, codec, custody};
    let root = f.temp.path().join("backup-root");
    fs::create_dir(&root).unwrap();
    drop(durable::lock(&root.join("lock"), false, true).unwrap());
    let backup = root.join("transition-source");
    fs::create_dir(&backup).unwrap();
    fs::write(backup.join("history"), b"retained source state").unwrap();
    let exe = std::env::current_exe().unwrap();
    let capture = json!([exe, "{backup}"]);
    let restore = json!([exe, "{backup}", "{workspace}"]);
    let config = json!({"version":1,"resource":"demo","root":root,"commands":{"capture":capture,"restore":restore,"verify":restore,"cleanup":restore},"executable_files":[],"timeout_seconds":10,"maximum_age_seconds":3600});
    durable::write_json(&f.temp.path().join("backup.json"), &config).unwrap();
    let mut tools = json!({});
    tools[exe.to_str().unwrap()] = json!(codec::file_digest(&exe).unwrap());
    let source = json!({"status":"verified","consistency":"quiesced","resource":"demo","manifest_sha256":application_backup::identity(&config).unwrap(),"executables":tools,"artifacts":application_backup::inventory(&backup).unwrap(),"captured_at":custody::now(),"semantic_sha256":"a".repeat(64),"executor_machine_sha256":"b".repeat(64),"executor":"source"});
    durable::write_json(&backup.join("acceptance.json"), &source).unwrap();
    let record = json!({"backup":backup,"source_acceptance_sha256":codec::file_digest(&backup.join("acceptance.json")).unwrap(),"semantic_sha256":source["semantic_sha256"],"independent_sha256":null});
    (source, record)
}
#[test]
fn stale_capture_can_abort_but_changed_bytes_or_evidence_keep_barriers() {
    use harbor_db::storage::{application_backup, custody};
    let f = Fixture::new();
    let (mut receipt, record) = captured_source(&f);
    let backup = std::path::Path::new(record["backup"].as_str().unwrap());
    receipt["captured_at"] = json!(custody::now() - 7200);
    durable::write_json(&backup.join("acceptance.json"), &receipt).unwrap();
    let config = durable::read_json(&f.temp.path().join("backup.json")).unwrap();
    assert!(application_backup::inspect(&config, backup, None).is_err());
    assert_eq!(
        application_backup::inspect_retained(&config, backup).unwrap(),
        receipt
    );
    fs::write(backup.join("history"), b"changed bytes").unwrap();
    assert!(application_backup::inspect_retained(&config, backup).is_err());
    fs::write(backup.join("history"), b"retained source state").unwrap();
    for at in [json!(-1), json!(true), json!(custody::now() + 3600)] {
        receipt["captured_at"] = at;
        durable::write_json(&backup.join("acceptance.json"), &receipt).unwrap();
        assert!(application_backup::inspect_retained(&config, backup).is_err());
    }
    let script = r#"
from test_application_transition import ApplicationTransitionTest
from harbor_db import application_transition as transition, application_backup, resource
from harbor_db.durable import read_json, write_json
from harbor_db.application_backup import digest
from pathlib import Path
import sys, time
f = ApplicationTransitionTest()
f.setUp()
try:
    f.evidence_patch.stop()
    backup = f.backup / 'capture'
    backup.mkdir()
    artifact = backup / 'history'
    artifact.write_bytes(b'retained source')
    tool = str(Path(sys.executable).resolve())
    config = {'version': 1, 'resource': 'demo', 'root': str(f.backup),
              'commands': {stage: [tool, '{backup}', '{workspace}'] for stage in ('capture', 'restore', 'verify', 'cleanup')},
              'executable_files': [], 'timeout_seconds': 10, 'maximum_age_seconds': 3600}
    write_json(Path(f.config['backup_manifest']), config)
    receipt = {**f.snapshot, 'manifest_sha256': application_backup.identity(config),
               'executables': application_backup.executables(config),
               'captured_at': int(time.time()) - 7200,
               'artifacts': {'history': digest(artifact)}}
    receipt_path = backup / 'acceptance.json'
    write_json(receipt_path, receipt)
    f.custody(); transition.plan(f.config, f.candidate, None)
    record = transition.status(f.config)
    record['backup'] = str(backup)
    record['semantic_sha256'] = receipt['semantic_sha256']
    record['source_acceptance_sha256'] = digest(receipt_path)
    transition.install_barriers(f.config, record)
    transition.save(f.config, record, 'preparing')
    barrier = f.barrier / 'inhibited.json'
    before = barrier.read_bytes()
    try: application_backup.inspect(read_json(f.config['backup_manifest']), backup)
    except ValueError as error: assert 'stale' in str(error)
    else: raise AssertionError('fresh admission accepted stale capture')
    retained_receipt = receipt_path.read_bytes()
    write_json(receipt_path, {**receipt, 'changed': True})
    try: transition.abort(f.config)
    except ValueError as error: assert 'source backup evidence changed' in str(error)
    else: raise AssertionError('changed receipt released writers')
    assert barrier.read_bytes() == before
    receipt_path.write_bytes(retained_receipt)
    artifact = next(backup / name for name in receipt['artifacts'] if name != 'capture.json')
    original = artifact.read_bytes()
    artifact.write_bytes(b'changed backup')
    try: transition.abort(f.config)
    except ValueError as error: assert 'hash' in str(error)
    else: raise AssertionError('changed bytes released writers')
    assert barrier.read_bytes() == before
    artifact.write_bytes(original)
    result = transition.abort(f.config)
    assert result['phase'] == 'aborted'
    assert not barrier.exists()
    assert receipt_path.read_bytes() == retained_receipt
    resource.verify(f.source, resource.contract(f.source))
finally: f.doCleanups()
"#;
    let diagnostic = format!(
        "import sys,traceback\ntry:\n exec({})\nexcept BaseException:\n traceback.print_exc(file=sys.stdout)\n raise\n",
        json!(script)
    );
    let mut spec =
        process::CommandSpec::new(vec!["python3".into(), "-B".into(), "-c".into(), diagnostic]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!(
                "{}/python:{}/tests",
                env!("CARGO_MANIFEST_DIR"),
                env!("CARGO_MANIFEST_DIR")
            ),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    let output = process::run(&spec).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn independent_restore_evidence_binds_bytes_tools_semantics_machine_and_resume() {
    use harbor_db::storage::{codec, custody};
    let f = Fixture::new();
    let (source, mut record) = captured_source(&f);
    let receipt = json!({"version":1,"status":"verified","resource":"demo","source_acceptance_sha256":record["source_acceptance_sha256"],"semantic_sha256":source["semantic_sha256"],"manifest_sha256":source["manifest_sha256"],"executables":source["executables"],"executor_machine_sha256":"c".repeat(64),"executor":"independent","certified_at":custody::now()});
    let path = f.temp.path().join("independent.json");
    durable::write_json(&path, &receipt).unwrap();
    let accepted = application_transition::evidence(&f.config, &record, &[]).unwrap();
    assert_eq!(accepted.0, source);
    record["independent_sha256"] = json!(accepted.1);
    for (key, value) in [
        ("semantic_sha256", json!("d".repeat(64))),
        ("manifest_sha256", json!("d".repeat(64))),
        ("executables", json!({})),
        ("executor", json!("source")),
        (
            "executor_machine_sha256",
            source["executor_machine_sha256"].clone(),
        ),
        ("resource", json!("another-resource")),
        ("source_acceptance_sha256", json!("d".repeat(64))),
        ("certified_at", json!(custody::now() + 60)),
        ("certified_at", json!(custody::now() - 3601)),
    ] {
        let mut changed = receipt.clone();
        changed[key] = value;
        durable::write_json(&path, &changed).unwrap();
        assert!(
            application_transition::evidence(&f.config, &record, &[]).is_err(),
            "{key}"
        );
    }
    let mut changed = receipt.clone();
    changed["certified_at"] = json!(receipt["certified_at"].as_i64().unwrap() - 1);
    durable::write_json(&path, &changed).unwrap();
    assert_ne!(
        codec::file_digest(&path).unwrap(),
        record["independent_sha256"]
    );
    assert!(
        application_transition::evidence(&f.config, &record, &[])
            .unwrap_err()
            .to_string()
            .contains("changed during resume")
    );
    durable::write_json(&path, &receipt).unwrap();
    let backup = std::path::Path::new(record["backup"].as_str().unwrap());
    fs::write(backup.join("history"), b"older source revision").unwrap();
    assert!(application_transition::source_bytes(&f.config, &record, &[]).is_err());
}
#[test]
fn source_acceptance_hash_is_bound_even_when_backup_remains_valid() {
    let f = Fixture::new();
    let (source, record) = captured_source(&f);
    let mut changed = source;
    changed["extra_retained_evidence"] = json!(true);
    durable::write_json(
        &std::path::Path::new(record["backup"].as_str().unwrap()).join("acceptance.json"),
        &changed,
    )
    .unwrap();
    assert!(
        application_transition::source_bytes(&f.config, &record, &[])
            .unwrap_err()
            .to_string()
            .contains("source backup evidence changed")
    );
}
#[test]
fn target_corpus_evidence_cannot_hide_behind_semantic_acceptance() {
    let f = Fixture::new();
    let root = std::path::Path::new(f.target["directories"][0].as_str().unwrap());
    fs::write(root.join("records"), b"source revision seven").unwrap();
    let entry = json!({"kind":"filesystem","authority":f.target,"custody_file":f.temp.path().join("authority/custody.json"),"database_inventory_checks":[]});
    let path = f.temp.path().join("custody.json");
    durable::write_json(&path, &entry).unwrap();
    let mut config = f.config.clone();
    config["custody_manifest"] = json!(path);
    let mut record = json!({"source_authority":{"identity":"retained"},"primary_snapshot_sha256":null,"source_acceptance_sha256":"a".repeat(64),"independent_sha256":"b".repeat(64),"semantic_sha256":"c".repeat(64)});
    record["custody"] =
        application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    application_transition::verify_custody(&config, &record, &f.target, &[]).unwrap();
    application_transition::semantic(
        &json!({"version":1,"status":"verified","semantic_sha256":"c".repeat(64)}),
        &"c".repeat(64),
    )
    .unwrap();
    fs::write(root.join("records"), b"source revision six!!").unwrap();
    assert!(
        application_transition::verify_custody(&config, &record, &f.target, &[])
            .unwrap_err()
            .to_string()
            .contains("corpus evidence changed")
    );
}
#[test]
fn reverified_transition_custody_returns_fresh_evidence_without_rebinding_contents_in_both_engines()
{
    let f = Fixture::new();
    fs::write(
        f.temp.path().join("new/records"),
        b"post-import revision seven",
    )
    .unwrap();
    let entry = json!({"kind":"filesystem","authority":f.target,"custody_file":f.temp.path().join("authority/custody.json"),"database_inventory_checks":[],"max_age_seconds":1});
    let path = f.temp.path().join("custody.json");
    durable::write_json(&path, &entry).unwrap();
    let mut config = f.config.clone();
    config["custody_manifest"] = json!(path);
    let mut record = json!({"source_authority":{"identity":"retained"},"primary_snapshot_sha256":null,"source_acceptance_sha256":"a".repeat(64),"independent_sha256":"b".repeat(64),"semantic_sha256":"c".repeat(64)});
    record["custody"] =
        application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    record["custody"]["completed_at"] = json!(harbor_db::storage::custody::now() - 60);
    let observed = serde_json::to_value(
        application_transition::verify_custody(&config, &record, &f.target, &[]).unwrap(),
    )
    .unwrap();
    let script = r#"import json,sys,time
from harbor_db import application_transition
c,r,t=map(json.loads,sys.argv[1:])
observed=application_transition.verify_custody(c,r,t,())
assert isinstance(observed,dict), 'fresh verified custody was discarded'
assert 0 <= int(time.time())-observed['completed_at'] <= 1
assert {k:v for k,v in observed.items() if k!='completed_at'} == {k:v for k,v in r['custody'].items() if k!='completed_at'}
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        record.to_string(),
        f.target.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    let peer = process::execute(&spec);
    assert!(observed.is_object(), "fresh verified custody was discarded");
    let completed = observed["completed_at"].as_i64().unwrap();
    assert!((0..=1).contains(&(harbor_db::storage::custody::now() - completed)));
    let mut expected = record["custody"].clone();
    expected["completed_at"] = json!(completed);
    assert_eq!(observed, expected);
    peer.unwrap();
    fs::write(
        f.temp.path().join("new/records"),
        b"changed during preparation",
    )
    .unwrap();
    assert!(application_transition::verify_custody(&config, &record, &f.target, &[]).is_err());
}

#[test]
fn custody_writers_must_be_covered_before_transition_or_inventory_in_both_engines() {
    let f = Fixture::new();
    let corpus = f.temp.path().join("new/records");
    fs::write(&corpus, b"acknowledged revision seven").unwrap();
    let path = f.temp.path().join("custody.json");
    let mut config = f.config.clone();
    config["custody_manifest"] = json!(path);
    config["retired_units"] = json!(["legacy.service"]);
    let mut entry = json!({"kind":"filesystem","authority":f.target,
        "custody_file":f.temp.path().join("authority/custody.json"),
        "database_inventory_checks":[],"runtime_units":["uncovered.service"]});
    durable::write_json(&path, &entry).unwrap();
    let source_before = fs::read(f.temp.path().join("source.json")).unwrap();
    let rejected = transition_manifest::validate(&config).unwrap_err();
    assert!(
        rejected.to_string().contains("custody writers"),
        "{rejected}"
    );
    let record = json!({"source_authority":{"identity":"retained"},"primary_snapshot_sha256":null,
        "source_acceptance_sha256":"a".repeat(64),"independent_sha256":"b".repeat(64),
        "semantic_sha256":"c".repeat(64)});
    assert!(
        application_transition::target_custody(&config, &record, &f.target, &[])
            .unwrap_err()
            .to_string()
            .contains("custody writers")
    );
    for writers in [
        json!(["demo.service", "uncovered.service"]),
        json!(null),
        json!([7]),
    ] {
        entry["runtime_units"] = writers;
        durable::write_json(&path, &entry).unwrap();
        assert!(
            transition_manifest::validate(&config)
                .unwrap_err()
                .to_string()
                .contains("custody writers")
        );
        assert!(
            application_transition::target_custody(&config, &record, &f.target, &[])
                .unwrap_err()
                .to_string()
                .contains("custody writers")
        );
    }
    assert!(!f.temp.path().join("authority/transition.json").exists());
    assert!(!f.temp.path().join("barrier/inhibited.json").exists());
    assert!(!f.temp.path().join("systemd").exists());
    assert_eq!(
        fs::read(f.temp.path().join("source.json")).unwrap(),
        source_before
    );
    assert_eq!(fs::read(&corpus).unwrap(), b"acknowledged revision seven");
    for runtime_units in [
        json!([]),
        json!(["demo.service"]),
        json!(["legacy.service"]),
        json!(["demo.service", "legacy.service"]),
    ] {
        entry["runtime_units"] = runtime_units;
        durable::write_json(&path, &entry).unwrap();
        transition_manifest::validate(&config).unwrap();
        application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    }
    let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_transition,transition_manifest
root_uid=int(sys.argv[4])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info); fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
config,record,target=map(json.loads,sys.argv[1:4])
path=Path(config['custody_manifest'])
entry=json.loads(path.read_text())
for units in (['uncovered.service'], ['demo.service','uncovered.service'], None, [7]):
    path.write_text(json.dumps(entry | {'runtime_units':units}))
    for operation in (lambda: transition_manifest.validate(config),
                      lambda: application_transition.target_custody(config,record,target,())):
        try: operation()
        except ValueError as error: assert 'custody writers' in str(error), error
        else: raise AssertionError('uncovered custody writer admitted')
for units in ([], ['demo.service'], ['legacy.service'], ['demo.service','legacy.service']):
    path.write_text(json.dumps(entry | {'runtime_units':units}))
    transition_manifest.validate(config)
    application_transition.target_custody(config,record,target,())
"#;
    let mut command = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        record.to_string(),
        f.target.to_string(),
        option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
    ]);
    command.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    process::execute(&command).unwrap();
}

#[test]
fn partial_or_extended_custody_cannot_authorize_transition_retries_in_both_engines() {
    let f = Fixture::new();
    fs::write(
        f.temp.path().join("new/records"),
        b"acknowledged revision seven",
    )
    .unwrap();
    let path = f.temp.path().join("custody.json");
    durable::write_json(&path, &json!({"kind":"filesystem","authority":f.target,
        "custody_file":f.temp.path().join("authority/custody.json"),"database_inventory_checks":[]})).unwrap();
    let mut config = f.config.clone();
    config["custody_manifest"] = json!(path);
    let mut record = json!({"source_authority":{"identity":"retained"},"primary_snapshot_sha256":null,
        "source_acceptance_sha256":"a".repeat(64),"independent_sha256":"b".repeat(64),"semantic_sha256":"c".repeat(64)});
    let complete =
        application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    let mut malformed = vec![
        json!({}),
        json!({"resource":"demo"}),
        json!([]),
        json!(true),
    ];
    for key in complete.as_object().unwrap().keys() {
        let mut partial = complete.clone();
        partial.as_object_mut().unwrap().remove(key);
        malformed.push(partial);
    }
    let mut extended = complete.clone();
    extended["undeclared"] = json!(true);
    malformed.push(extended);
    for timestamp in [
        json!(true),
        json!(-1),
        json!(null),
        json!(harbor_db::storage::custody::now() + 3600),
    ] {
        let mut changed = complete.clone();
        changed["completed_at"] = timestamp;
        malformed.push(changed);
    }
    for custody in malformed {
        record["custody"] = custody;
        assert!(
            application_transition::verify_custody(&config, &record, &f.target, &[]).is_err(),
            "partial custody was admitted: {}",
            record["custody"]
        );
    }
    record["custody"] = complete;
    application_transition::verify_custody(&config, &record, &f.target, &[]).unwrap();
    let script = r#"import copy
from test_application_transition import ApplicationTransitionTest
from harbor_db import application_transition as transition
from harbor_db.durable import read_json,write_json
f=ApplicationTransitionTest();f.setUp()
try:
    f.custody();f.prepare()
    journal=f.state/'transition.json';retained=read_json(journal)
    variants=[{}, {'resource':'demo'}, [], True]
    variants += [{k:v for k,v in retained['custody'].items() if k!=missing} for missing in retained['custody']]
    variants.append(retained['custody'] | {'undeclared':True})
    variants += [retained['custody'] | {'completed_at':at} for at in (True,-1,None,retained['custody']['completed_at']+3600)]
    for value in variants:
        changed=copy.deepcopy(retained);changed['custody']=value
        for operation in (transition.prepare,transition.commit):
            write_json(journal,changed)
            identity=(f.state/'identity.json').read_bytes();custody=(f.state/'custody.json').read_bytes()
            try: operation(f.config)
            except ValueError: pass
            else: raise AssertionError('partial custody admitted: '+repr(value))
            assert (f.state/'identity.json').read_bytes()==identity
            assert (f.state/'custody.json').read_bytes()==custody
    write_json(journal,retained)
    transition.prepare(f.config);transition.commit(f.config)
finally: f.doCleanups()
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!(
                "{}/python:{}/tests",
                env!("CARGO_MANIFEST_DIR"),
                env!("CARGO_MANIFEST_DIR")
            ),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    process::execute(&spec).unwrap();
}

#[test]
fn rejected_barrier_release_preserves_all_markers_and_startup_policy_in_both_engines() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let mut config = f.config.clone();
    config["retired_units"] = json!(["legacy.service"]);
    let record = json!({"intent":"selected-intent","barrier_candidate":"selected-candidate",
        "source_generation":"retained-source","candidate":"selected-target"});
    let state = f.temp.path().join("barrier");
    fs::create_dir(state.join("retired")).unwrap();
    let policy = state.join("start-policy.json");
    let primary = state.join("inhibited.json");
    let retired = state.join("retired/inhibited.json");
    let expected_primary =
        json!({"intent":record["intent"],"candidate":record["barrier_candidate"]});
    let expected_retired = json!({"resource":"demo","units":["legacy.service"]});
    for aborted in [false, true] {
        for bad_retired in [false, true] {
            durable::write_json(&policy, &json!({"retain":"original startup authority"})).unwrap();
            durable::write_json(
                &primary,
                &if bad_retired {
                    expected_primary.clone()
                } else {
                    json!({"foreign":true})
                },
            )
            .unwrap();
            durable::write_json(
                &retired,
                &if bad_retired {
                    json!({"foreign":true})
                } else {
                    expected_retired.clone()
                },
            )
            .unwrap();
            let before: Vec<_> = [&policy, &primary, &retired]
                .iter()
                .map(|path| (fs::read(path).unwrap(), fs::metadata(path).unwrap().ino()))
                .collect();
            assert!(transition_manifest::release_barriers(&config, &record, aborted).is_err());
            for (path, (bytes, inode)) in [&policy, &primary, &retired].iter().zip(before) {
                assert_eq!(fs::read(path).unwrap(), bytes);
                assert_eq!(fs::metadata(path).unwrap().ino(), inode);
            }
        }
    }
    durable::write_json(&primary, &expected_primary).unwrap();
    durable::write_json(&retired, &expected_retired).unwrap();
    transition_manifest::release_barriers(&config, &record, true).unwrap();
    assert!(!primary.exists() && !retired.exists());
    assert_eq!(
        durable::read_json(&policy).unwrap()["generation"],
        record["source_generation"]
    );
    transition_manifest::release_barriers(&config, &record, true).unwrap();
    let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import transition_manifest
from harbor_db.durable import write_json,read_json
uid=int(sys.argv[3])
if uid:
    original=Path.lstat
    def mapped(path):
        info=original(path)
        if info.st_uid!=uid: return info
        fields=list(info);fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped).start()
c,r=map(json.loads,sys.argv[1:3]);state=Path(c['barrier_dir'])
policy=state/'start-policy.json';primary=state/'inhibited.json';retired=state/'retired/inhibited.json'
p={'intent':r['intent'],'candidate':r['barrier_candidate']};q={'resource':c['resource'],'units':c['retired_units']}
for aborted,bad_retired in ((False,False),(False,True),(True,False),(True,True)):
    write_json(policy,{'retain':'original startup authority'})
    write_json(primary,p if bad_retired else {'foreign':True});write_json(retired,{'foreign':True} if bad_retired else q)
    before=[(path.read_bytes(),path.stat().st_ino) for path in (policy,primary,retired)]
    try: transition_manifest.release_barriers(c,r,aborted=aborted)
    except ValueError: pass
    else: raise AssertionError('foreign startup barrier released')
    assert before==[(path.read_bytes(),path.stat().st_ino) for path in (policy,primary,retired)]
write_json(primary,p);write_json(retired,q)
transition_manifest.release_barriers(c,r,aborted=True)
assert not primary.exists() and not retired.exists()
assert read_json(policy)['generation']==r['source_generation']
transition_manifest.release_barriers(c,r,aborted=True)
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        record.to_string(),
        option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0").into(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    process::execute(&spec).unwrap();
}
#[test]
fn rejected_barrier_installation_preserves_policy_markers_and_drop_ins_in_both_engines() {
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::Path,
    };
    if unsafe { libc::geteuid() } != 0 {
        let root = tempfile::tempdir().unwrap();
        // A private filesystem root gives namespace-root-owned ancestors, without
        // changing host ownership or bypassing the production ownership checks.
        let launch = r#"import os,subprocess,sys
from pathlib import Path
sys.stderr=sys.stdout
root=Path(sys.argv[1]);source=Path(sys.argv[2]);binary=sys.argv[3]
# Namespace continuation must also retain executables from an external target.
paths=[Path('/nix'),Path('/proc'),Path('/dev'),source,Path(binary).parent]
# Registrations without F resolve the interpreter again inside this chroot.
if Path('/run/binfmt').is_dir(): paths.append(Path('/run/binfmt'))
for path in paths:
    target=root/path.relative_to('/');target.mkdir(parents=True,exist_ok=True)
    subprocess.run(['mount','--rbind',str(path),str(target)],check=True,stderr=subprocess.STDOUT)
    subprocess.run(['mountpoint','-q',str(target)],check=True,stderr=subprocess.STDOUT)
(root/'tmp').mkdir(mode=0o1777)
(root/'etc').mkdir()
(root/'etc/passwd').write_text('root:x:0:0:fixture:/tmp:/bin/sh\n')
(root/'etc/group').write_text('root:x:0:\n')
os.chroot(root);os.chdir(source);os.environ['TMPDIR']='/tmp'
os.execv(binary,[binary,*sys.argv[4:]])
"#;
        let mut spec = process::CommandSpec::new(vec![
            "unshare".into(),
            "--user".into(),
            "--map-root-user".into(),
            "--mount".into(),
            "--".into(),
            "python3".into(),
            "-B".into(),
            "-c".into(),
            launch.into(),
            root.path().display().to_string(),
            env!("CARGO_MANIFEST_DIR").into(),
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "rejected_barrier_installation_preserves_policy_markers_and_drop_ins_in_both_engines"
                .into(),
            "--nocapture".into(),
        ]);
        spec.timeout = std::time::Duration::from_secs(120);
        let output = process::run(&spec).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    std::panic::set_hook(Box::new(|failure| {
        println!("barrier qualification failure: {failure}")
    }));
    let f = Fixture::new();
    let mut config = f.config.clone();
    config["retired_units"] = json!(["legacy.service"]);
    let record = json!({"intent":"selected-intent","barrier_candidate":"selected-candidate", "source_generation":"selected-source"});
    let state = Path::new(config["barrier_dir"].as_str().unwrap());
    fs::create_dir(state.join("retired")).unwrap();
    let policy = state.join("start-policy.json");
    let primary = state.join("inhibited.json");
    let retired = state.join("retired/inhibited.json");
    let drop_ins = f.temp.path().join("systemd");
    fs::create_dir(&drop_ins).unwrap();
    let owned_dir = drop_ins.join("retained.service.d");
    fs::create_dir(&owned_dir).unwrap();
    let retained_drop_in = owned_dir.join("zzzz-harbor-db-backend-transition.conf");
    fs::write(&retained_drop_in, "retained original condition").unwrap();
    let command = f.temp.path().join("readback.py");
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(&command, format!("#!{}\nimport json,sys\nfrom pathlib import Path\nr=Path({:?})\nif sys.argv[1]=='daemon-reload': (r/'reloaded').write_text('yes')\nelif sys.argv[1]=='show': print(r/'systemd'/(sys.argv[2]+'.d')/'zzzz-harbor-db-backend-transition.conf')\nelse:\n state=r/'barrier'\n if 'legacy' in sys.argv[4]: state=state/'retired'\n print(json.dumps({{'type':'a(sbbsi)','data':[['ConditionPathExists',False,True,str(state/'inhibited.json'),0]]}}))\n",python.display(),f.temp.path().display().to_string())).unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
    config["systemctl"] = json!(command);
    config["busctl"] = json!(command);
    let expected_primary =
        json!({"intent":record["intent"],"candidate":record["barrier_candidate"]});
    let expected_retired = json!({"resource":"demo","units":["legacy.service"]});
    let mut failures = Vec::new();
    for bad_retired in [false, true] {
        durable::write_json(&policy, &json!({"retain":"legitimate startup policy"})).unwrap();
        durable::write_json(
            &primary,
            &if bad_retired {
                expected_primary.clone()
            } else {
                json!({"intent":"foreign","candidate":null})
            },
        )
        .unwrap();
        durable::write_json(
            &retired,
            &if bad_retired {
                json!({"resource":"foreign","units":[]})
            } else {
                expected_retired.clone()
            },
        )
        .unwrap();
        let before: Vec<_> = [&policy, &primary, &retired, &retained_drop_in]
            .iter()
            .map(|p| (fs::read(p).unwrap(), fs::metadata(p).unwrap().ino()))
            .collect();
        let rejected = transition_manifest::install_barriers(&config, &record).is_err();
        let preserved = [&policy, &primary, &retired, &retained_drop_in]
            .iter()
            .zip(before)
            .all(|(p, (b, i))| fs::read(p).unwrap() == b && fs::metadata(p).unwrap().ino() == i);
        failures.push((
            bad_retired,
            rejected,
            preserved,
            !drop_ins.join("demo.service.d").exists(),
            !drop_ins.join("legacy.service.d").exists(),
            !f.temp.path().join("reloaded").exists(),
        ));
    }
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import transition_manifest
from harbor_db.durable import write_json
sys.stderr=sys.stdout
c,r=map(json.loads,sys.argv[1:]);state=Path(c['barrier_dir']);policy=state/'start-policy.json';primary=state/'inhibited.json';retired=state/'retired/inhibited.json';dropins=Path(c['drop_in_root'])
failures=[]
for bad_retired in (False,True):
    write_json(policy,{'retain':'legitimate startup policy'})
    write_json(primary,{'intent':r['intent'],'candidate':r['barrier_candidate']} if bad_retired else {'intent':'foreign','candidate':None})
    write_json(retired,{'resource':'foreign','units':[]} if bad_retired else {'resource':'demo','units':['legacy.service']})
    before=[(p.read_bytes(),p.stat().st_ino) for p in (policy,primary,retired)]
    try: transition_manifest.install_barriers(c,r)
    except ValueError: rejected=True
    else: rejected=False
    failures.append((bad_retired,rejected,before==[(p.read_bytes(),p.stat().st_ino) for p in (policy,primary,retired)],not (dropins/'new.service.d').exists()))
assert all(all(result[1:]) for result in failures),failures
write_json(primary,{'intent':r['intent'],'candidate':r['barrier_candidate']})
write_json(retired,{'resource':c['resource'],'units':c['retired_units']})
transition_manifest.install_barriers(c,r)
transition_manifest.install_barriers(c,r)
"#;
    let mut python_config = config.clone();
    python_config["units"] = json!(["new.service"]);
    let mut spec = process::CommandSpec::new(vec![
        python.display().to_string(),
        "-B".into(),
        "-c".into(),
        script.into(),
        python_config.to_string(),
        record.to_string(),
    ]);
    spec.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    let python_result = process::run(&spec).unwrap();
    assert!(
        failures.iter().all(
            |(_, rejected, preserved, no_primary, no_retired, no_reload)| *rejected
                && *preserved
                && *no_primary
                && *no_retired
                && *no_reload
        ),
        "{failures:?}; Python: {python_result:?}"
    );
    assert!(
        python_result.status.success(),
        "{}",
        String::from_utf8_lossy(&python_result.stdout)
    );
    durable::write_json(&primary, &expected_primary).unwrap();
    durable::write_json(&retired, &expected_retired).unwrap();
    transition_manifest::install_barriers(&config, &record).unwrap();
    transition_manifest::install_barriers(&config, &record).unwrap();
    assert_eq!(
        durable::read_json(&policy).unwrap()["generation"],
        record["source_generation"]
    );
}
#[test]
fn backup_pin_and_borrowed_fence_require_declared_retained_anchors() {
    let f = Fixture::new();
    let (_, mut record) = captured_source(&f);
    record["phase"] = json!("captured");
    let pin = application_transition::pin_source(&f.config, &record)
        .unwrap()
        .unwrap();
    let root = f.temp.path().join("backup-root");
    assert!(durable::lock(&root.join("lock"), false, false).is_err());
    drop(pin);
    record["phase"] = json!("write-enabled");
    assert!(
        application_transition::pin_source(&f.config, &record)
            .unwrap()
            .is_none()
    );
    durable::lock(&root.join("lock"), false, false).unwrap();
    let fds = transition_manifest::fence(&f.config, &mut record, &[]).unwrap();
    assert!(fds.fds.is_empty());
    assert!(fds.leases.is_empty());
}

#[test]
fn interrupted_write_enabled_release_keeps_the_source_backup_pinned() {
    let f = Fixture::new();
    let (_, mut record) = captured_source(&f);
    record["phase"] = json!("write-enabled");
    let marker = f.temp.path().join("barrier/inhibited.json");
    durable::write_json(&marker, &json!({"intent":"pending-release"})).unwrap();
    let before = fs::read(&marker).unwrap();
    let lease = application_transition::pin_source(&f.config, &record)
        .unwrap()
        .expect("a journal phase cannot release an inhibited transition's source pin");
    assert!(durable::lock(&f.temp.path().join("backup-root/lock"), false, false).is_err());
    assert_eq!(fs::read(&marker).unwrap(), before);
    drop(lease);
    fs::remove_file(&marker).unwrap();
    assert!(
        application_transition::pin_source(&f.config, &record)
            .unwrap()
            .is_none()
    );
}

#[test]
fn python_interrupted_release_revalidates_and_preserves_barrier_until_retry() {
    let script = r#"
import fcntl
from unittest import mock
from test_application_transition import ApplicationTransitionTest
from harbor_db import application_transition as transition
from harbor_db.durable import lock, read_json, write_json
f = ApplicationTransitionTest()
f.setUp()
try:
    f.custody()
    f.prepare()
    transition.commit(f.config)
    with mock.patch.object(transition, 'release_barriers', side_effect=OSError('interrupted')):
        try:
            transition.enable_writes(f.config)
        except OSError:
            pass
        else:
            raise AssertionError('interruption was not exercised')
    assert transition.status(f.config)['phase'] == 'write-enabled'
    marker = f.barrier / 'inhibited.json'
    before = marker.read_bytes()
    with transition.pin_source(f.config, transition.status(f.config), []) as leases:
        assert len(leases) == 1
        with open(f.backup / 'lock', 'rb') as contender:
            try:
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError('pending release lost the source backup pin')
    proof_path = f.root / 'independent.json'
    proof = read_json(proof_path)
    write_json(proof_path, proof | {'semantic_sha256':'d'*64})
    try:
        transition.enable_writes(f.config)
    except ValueError:
        pass
    else:
        raise AssertionError('changed independent proof released writers')
    write_json(proof_path, proof)
    unaccepted = f.new / 'unaccepted'
    unaccepted.write_bytes(b'corpus drift')
    try:
        transition.enable_writes(f.config)
    except ValueError:
        pass
    else:
        raise AssertionError('changed target corpus released writers')
    try:
        transition.complete(f.config)
    except ValueError as error:
        assert 'released writer barriers' in str(error)
    else:
        raise AssertionError('inhibited transition completed')
    assert marker.read_bytes() == before
    unaccepted.unlink()
    transition.enable_writes(f.config)
    assert not marker.exists()
    (f.new / 'records').write_bytes(b'acknowledged post-release change')
    with lock(f.state / 'lock', shared=True):
        transition.enable_writes(f.config)
    f.actions.return_value = {'version':1,'status':'healthy'}
    transition.complete(f.config)
finally:
    f.doCleanups()
"#;
    let mut command = std::process::Command::new("python3");
    command.args(["-B", "-c", script]).env(
        "PYTHONPATH",
        format!("{0}/python:{0}/tests", env!("CARGO_MANIFEST_DIR")),
    );
    let output = process::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
#[test]
fn redirected_barrier_storage_and_writable_ancestors_reject_manifest() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let link = f.temp.path().join("redirected");
    symlink(f.temp.path().join("barrier"), &link).unwrap();
    let mut config = f.config.clone();
    config["barrier_dir"] = json!(link);
    assert!(
        transition_manifest::validate(&config)
            .unwrap_err()
            .to_string()
            .contains("redirected")
    );
    let parent = f.temp.path().join("untrusted");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    config["barrier_dir"] = json!(parent.join("barrier"));
    assert!(
        transition_manifest::validate(&config)
            .unwrap_err()
            .to_string()
            .contains("untrusted ancestor")
    );
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn generation_etc_contract_cannot_escape_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let candidate = store.join("generation");
    let bundle = store.join("etc-bundle");
    fs::create_dir_all(&candidate).unwrap();
    fs::create_dir_all(bundle.join("harbor-db")).unwrap();
    symlink(&bundle, candidate.join("etc")).unwrap();
    let manifest = store.join("manifest.json");
    let expected = json!({"version":1,"resource":"demo"});
    durable::write_json(&manifest, &expected).unwrap();
    let link = bundle.join("harbor-db/demo-transition.json");
    symlink(&manifest, &link).unwrap();
    assert_eq!(
        transition_manifest::generation_contract(&candidate, "demo", &store).unwrap(),
        expected
    );
    fs::remove_file(&link).unwrap();
    let outside = temp.path().join("mutable.json");
    durable::write_json(&outside, &expected).unwrap();
    symlink(outside, link).unwrap();
    assert!(transition_manifest::generation_contract(&candidate, "demo", &store).is_err());
}

#[test]
fn semantic_acceptance_is_exact_and_complete() {
    let expected = "a".repeat(64);
    let good = json!({"version":1,"status":"verified","semantic_sha256":expected});
    application_transition::semantic(&good, &expected).unwrap();
    let mut extra = good.clone();
    extra["unverified"] = json!(true);
    assert!(application_transition::semantic(&extra, &expected).is_err());
    assert!(application_transition::semantic(&good, &"b".repeat(64)).is_err());
}

#[test]
fn target_custody_inventory_checks_default_only_when_absent() {
    let f = Fixture::new();
    let root = std::path::Path::new(f.target["directories"][0].as_str().unwrap());
    fs::write(root.join("records"), b"retained application records").unwrap();
    let path = f.temp.path().join("custody.json");
    let entry = json!({
        "kind":"filesystem", "authority":f.target,
        "custody_file":f.temp.path().join("authority/custody.json")
    });
    let mut config = f.config.clone();
    config["custody_manifest"] = json!(path);
    let record = json!({
        "source_authority":{"identity":"retained"},
        "primary_snapshot_sha256":null,
        "source_acceptance_sha256":"a".repeat(64),
        "independent_sha256":"b".repeat(64),
        "semantic_sha256":"c".repeat(64)
    });
    durable::write_json(&path, &entry).unwrap();
    let absent = application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    assert_eq!(absent["database_requirements"], json!([]));
    let mut empty = entry.clone();
    empty["database_inventory_checks"] = json!([]);
    durable::write_json(&path, &empty).unwrap();
    let accepted =
        application_transition::target_custody(&config, &record, &f.target, &[]).unwrap();
    assert_eq!(accepted["database_requirements"], json!([]));
    assert_eq!(accepted["inventory"], absent["inventory"]);
    for malformed in [json!(null), json!("not-an-array"), json!({})] {
        let mut changed = entry.clone();
        changed["database_inventory_checks"] = malformed.clone();
        durable::write_json(&path, &changed).unwrap();
        assert!(
            application_transition::target_custody(&config, &record, &f.target, &[]).is_err(),
            "malformed inventory checks accepted: {malformed}"
        );
    }
}
