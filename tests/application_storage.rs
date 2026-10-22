use harbor_db::storage::{application_backup, provision};
use harbor_db::storage::{codec, durable, process};
use serde_json::Value;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
const ADAPTER: &str = r#"import hashlib,json,pathlib,sys,os,fcntl
operation,backup,workspace=sys.argv[1:]
backup,workspace=pathlib.Path(backup),pathlib.Path(workspace)
base=pathlib.Path(__file__).parent
assert os.environ['HARBOR_DB_APPLICATION_TIMEOUT_SECONDS']=='10'
fds=[int(s) for s in os.environ['HARBOR_DB_LEASE_FDS'].split(',')]
assert fds and all(fd>=3 for fd in fds)
for fd in fds: os.fstat(fd)
for path in (workspace.parent/'lock',base/'extra.lock'):
    if path.exists():
        with open(path) as lock:
            try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError: pass
            else: raise AssertionError('worker did not retain lease')
if operation=='capture':
    backup.mkdir()
    (backup/'records.json').write_text('[{"id":1,"revision":7}]')
    if (base/'hardlink').exists():
        (backup/'records.json').unlink()
        os.link(base/'mutable.json', backup/'records.json')
    semantic=hashlib.sha256((backup/'records.json').read_bytes()).hexdigest()
    (backup/'capture.json').write_text(json.dumps({'version':1,'consistency':'quiesced','semantic_sha256':semantic}))
    if (base/'symlink').exists(): (backup/'redirect').symlink_to('/etc/passwd')
    if (base/'self_accept').exists(): (backup/'acceptance.json').write_text('{}')
    if (base/'incomplete_semantic').exists(): (backup/'capture.json').write_text('{}')
    if (base/'oversized_receipt').exists():
        with (backup/'capture.json').open('r+b') as control:
            control.seek(0,2)
            control.write(b' ' * 4096)
    if (base/'receipt_size').exists():
        size=int((base/'receipt_size').read_text())
        with (backup/'capture.json').open('r+b') as control:
            length=control.seek(0,2)
            if size <= 4096: control.write(b' ' * (size-length))
            else: control.truncate(size)
    if (base/'non_utf8').exists():
        for suffix in (b'\xff', b'\xfe'):
            with open(os.fsencode(backup) + b'/artifact-' + suffix, 'wb') as artifact: artifact.write(suffix)
    if (base/'empty_directory').exists(): (backup/'required-empty'/'nested').mkdir(parents=True)
elif operation=='restore':
    if (base/'fail_restore').exists(): sys.exit(1)
    (workspace/'restored.json').write_bytes((backup/'records.json').read_bytes())
elif operation=='verify':
    assert (workspace/'restored.json').read_bytes()==(backup/'records.json').read_bytes()
    semantic=hashlib.sha256((workspace/'restored.json').read_bytes()).hexdigest()
    if (base/'wrong_semantic').exists(): semantic='0'*64
    print(json.dumps({'version':1,'status':'verified','semantic_sha256':semantic}))
    if (base/'mutate').exists(): (backup/'records.json').write_text('[]')
elif operation=='cleanup':
    if (base/'slow_cleanup').exists():
        import time
        time.sleep(7.1)
    if (base/'fail_cleanup').exists(): sys.exit(1)
    if (base/'mutate_cleanup').exists(): (backup/'records.json').write_text('[]')
elif operation=='fail': sys.exit(1)
"#;
fn backup_fixture() -> (tempfile::TempDir, Value) {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("backups")).unwrap();
    fs::write(t.path().join("backups/lock"), b"").unwrap();
    let adapter = t.path().join("adapter.py");
    fs::write(&adapter, ADAPTER).unwrap();
    let python = std::env::var("HARBOR_DB_TEST_PYTHON").unwrap_or_else(|_| {
        std::env::split_paths(
            &std::env::var_os("PATH").expect("approved test environment has PATH"),
        )
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .expect("approved test environment provides python3")
        .to_string_lossy()
        .into_owned()
    });
    let mut commands = json!({});
    for stage in ["capture", "restore", "verify", "cleanup"] {
        commands[stage] = json!([python, adapter, stage, "{backup}", "{workspace}"]);
    }
    let c = json!({"version":1,"resource":"demo","root":t.path().join("backups"),"timeout_seconds":10,"maximum_age_seconds":3600,"commands":commands,"executable_files":[adapter]});
    (t, c)
}
fn last(t: &Path) -> Value {
    durable::read_json(&t.join("backups/LAST_SUCCESS")).unwrap()
}

fn backup_cli(python: bool) -> std::process::Command {
    if !python {
        return std::process::Command::new(env!("CARGO_BIN_EXE_harbor-db-application-backup"));
    }
    let mut command = std::process::Command::new("python3");
    command.args([
        "-B",
        "-c",
        r#"import sys
from unittest.mock import patch
from harbor_db import application_backup
fixture = sys.argv.pop(1)
if fixture:
    original_digest = application_backup.digest
    patch.object(application_backup, 'machine', side_effect=lambda: original_digest(fixture)).start()
raise SystemExit(application_backup.main())
"#,
        option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or(""),
    ]);
    command.env(
        "PYTHONPATH",
        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
    );
    command
}

#[test]
fn oversized_capture_control_rejects_before_restore_and_preserves_success_in_both_engines() {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let capture = |attempt: &str| {
            if python {
                backup_peer(&c, "capture", attempt, false)
            } else {
                application_backup::capture(&c, attempt, false)
            }
        };
        capture("before-control-attack").unwrap();
        let before = fs::read(t.path().join("backups/LAST_SUCCESS")).unwrap();
        fs::write(t.path().join("oversized_receipt"), "").unwrap();
        // A valid, small JSON object followed by whitespace must be rejected by
        // the read limit, rather than eventually failing schema validation.
        let error = capture("oversized-control").unwrap_err().to_string();
        assert!(error.contains("capture receipt exceeds"), "{error}");
        assert!(!t.path().join("backups/oversized-control").exists());
        assert!(
            !t.path()
                .join("backups/.oversized-control.restore/restored.json")
                .exists()
        );
        assert_eq!(
            before,
            fs::read(t.path().join("backups/LAST_SUCCESS")).unwrap()
        );
        fs::remove_file(t.path().join("oversized_receipt")).unwrap();
        for size in [4097_u64, 1 << 40] {
            fs::write(t.path().join("receipt_size"), size.to_string()).unwrap();
            let attempt = format!("sparse-control-{size}");
            let error = capture(&attempt).unwrap_err().to_string();
            assert!(error.contains("capture receipt exceeds"), "{error}");
            assert!(!t.path().join(format!("backups/{attempt}")).exists());
            assert_eq!(
                before,
                fs::read(t.path().join("backups/LAST_SUCCESS")).unwrap()
            );
        }
        fs::write(t.path().join("receipt_size"), "4096").unwrap();
        capture("exact-control-limit").unwrap();
    }
}

#[test]
fn protected_backup_admits_only_immutable_command_and_adapter_files_in_both_engines() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let (t, mut config) = backup_fixture();
    config["version"] = json!(2);
    config["worker_user"] = json!("nobody");
    let executable = fs::canonicalize(config["commands"]["capture"][0].as_str().unwrap()).unwrap();
    config["executable_files"] = json!([]);
    for stage in ["capture", "restore", "verify", "cleanup"] {
        config["commands"][stage] = json!([executable, "{backup}", "{workspace}"]);
    }
    let mutable = t.path().join("worker-adapter");
    fs::write(&mutable, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&mutable, fs::Permissions::from_mode(0o755)).unwrap();
    let alias = t.path().join("store-alias");
    symlink(&executable, &alias).unwrap();
    let anchor = t.path().join("backups/lock");
    let original = fs::metadata(&anchor).unwrap();
    let mut rejected = Vec::new();
    for stage in ["capture", "restore", "verify", "cleanup"] {
        for path in [&mutable, &alias, t.path(), &t.path().join("absent")] {
            let mut selected = config.clone();
            selected["commands"][stage][0] = json!(path);
            rejected.push(selected);
        }
        if let Some(file) = option_env!("HARBOR_DB_TEST_MACHINE_ID") {
            let mut selected = config.clone();
            selected["commands"][stage][0] = json!(file);
            rejected.push(selected);
        }
    }
    let mut selected = config.clone();
    selected["executable_files"] = json!([mutable]);
    rejected.push(selected);
    for selected in &rejected {
        let result = application_backup::validate(selected, true);
        assert!(
            result.is_err(),
            "protected worker-controlled file admitted: {selected}"
        );
    }
    if let Some(file) = option_env!("HARBOR_DB_TEST_MACHINE_ID") {
        config["executable_files"] = json!([file]);
    }
    application_backup::validate(&config, true).unwrap();
    let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_backup
config,rejected=json.loads(sys.argv[1]),json.loads(sys.argv[2]);uid=int(sys.argv[3])
if uid:
    original=Path.lstat
    def mapped(path):
        info=original(path)
        if info.st_uid!=uid: return info
        fields=list(info);fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped).start()
for selected in rejected:
    try: application_backup.validate(selected)
    except (OSError,ValueError): pass
    else: raise AssertionError('protected worker-controlled file admitted')
application_backup.validate(config)
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        serde_json::to_string(&rejected).unwrap(),
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
    let retained = fs::metadata(&anchor).unwrap();
    assert_eq!(
        (original.dev(), original.ino()),
        (retained.dev(), retained.ino())
    );
    assert_eq!(fs::read(&mutable).unwrap(), b"#!/bin/sh\nexit 0\n");
    assert_eq!(fs::read_dir(t.path().join("backups")).unwrap().count(), 1);
}

#[test]
fn automatic_manual_capture_remains_pinned_after_scheduled_retention_in_both_engines() {
    let mut outcomes = Vec::new();
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let config = t.path().join("config.json");
        durable::write_json(&config, &c).unwrap();
        let mut command = backup_cli(python);
        let result = command
            .arg("--config")
            .arg(&config)
            .arg("capture")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let receipt: Value = serde_json::from_slice(&result.stdout).unwrap();
        let id = receipt["attempt"].as_str().unwrap();
        let root = t.path().join("backups");
        let original = fs::read(root.join(id).join("acceptance.json")).unwrap();
        let scheduled = command.arg("--scheduled").output().unwrap();
        assert!(
            scheduled.status.success(),
            "{}",
            String::from_utf8_lossy(&scheduled.stderr)
        );
        let scheduled: Value = serde_json::from_slice(&scheduled.stdout).unwrap();
        assert!(scheduled["attempt"].as_str().unwrap().starts_with("timer-"));
        // Equal capture clocks still rank newer IDs after the automatic manual
        // point. Exercise the formerly ambiguous timestamp/PID shape too.
        for name in [
            "20990101T000000Z-1",
            "timer-20990101T000000Z-1",
            "timer-20990101T000001Z-1",
        ] {
            application_backup::capture(&c, name, false).unwrap();
        }
        assert!(prune_peer(&c, python, 1).status.success());
        outcomes.push((
            python,
            fs::read(root.join(id).join("acceptance.json")).ok() == Some(original),
            root.join("20990101T000000Z-1/acceptance.json").is_file(),
            !root.join("timer-20990101T000000Z-1").exists(),
            root.join("timer-20990101T000001Z-1/acceptance.json")
                .is_file(),
        ));
    }
    assert!(
        outcomes
            .iter()
            .all(|(_, manual, legacy, pruned, latest)| *manual && *legacy && *pruned && *latest),
        "{outcomes:?}"
    );
}

fn prune_peer(c: &Value, python: bool, keep: u32) -> std::process::Output {
    let t = tempfile::tempdir().unwrap();
    let config = t.path().join("config.json");
    durable::write_json(&config, c).unwrap();
    let mut command = backup_cli(python);
    command
        .arg("--config")
        .arg(config)
        .args([
            "prune",
            "--keep",
            &keep.to_string(),
            "--incomplete-age-seconds",
            "120",
        ])
        .output()
        .unwrap()
}

#[test]
fn scheduled_retention_bounds_success_and_incomplete_attempts_without_pruning_transitions_in_both_engines()
 {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let root = t.path().join("backups");
        for id in [
            "timer-20260101T000000Z-1",
            "timer-20260101T000001Z-1",
            "timer-20260101T000002Z-1",
            "transition-fixed-run",
        ] {
            application_backup::capture(&c, id, false).unwrap();
        }
        let original = fs::read(root.join("LAST_SUCCESS")).unwrap();
        for id in ["timer-20260101T000003Z-1", "transition-incomplete"] {
            fs::write(t.path().join("fail_restore"), b"").unwrap();
            assert!(application_backup::capture(&c, id, false).is_err());
            for suffix in [".partial", ".intent.json"] {
                let path = root.join(format!("{id}{suffix}"));
                fs::File::open(path)
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new().set_modified(
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
                        ),
                    )
                    .unwrap();
            }
        }
        let output = prune_peer(&c, python, 2);
        assert!(
            output.status.success(),
            "python={python}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("timer-20260101T000000Z-1").exists());
        assert!(!root.join("timer-20260101T000000Z-1.intent.json").exists());
        assert!(!root.join("timer-20260101T000003Z-1.partial").exists());
        assert!(
            root.join("timer-20260101T000001Z-1/acceptance.json")
                .is_file()
        );
        assert!(
            root.join("timer-20260101T000002Z-1/acceptance.json")
                .is_file()
        );
        assert!(root.join("transition-fixed-run/acceptance.json").is_file());
        assert!(root.join("transition-incomplete.partial").is_dir());
        assert_eq!(fs::read(root.join("LAST_SUCCESS")).unwrap(), original);
        assert!(prune_peer(&c, python, 2).status.success());
    }
}

#[test]
fn retained_success_prunes_its_incomplete_siblings_without_changing_evidence_in_both_engines() {
    use std::os::unix::fs::MetadataExt;
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let root = t.path().join("backups");
        let id = "timer-20260101T000000Z-1";
        application_backup::capture(&c, id, false).unwrap();
        let acceptance = root.join(id).join("acceptance.json");
        let marker = root.join("LAST_SUCCESS");
        let retained = (
            fs::read(&acceptance).unwrap(),
            fs::metadata(&acceptance).unwrap().ino(),
            fs::read(&marker).unwrap(),
            fs::metadata(&marker).unwrap().ino(),
        );
        for suffix in [
            "partial",
            "sealed",
            "credentials",
            "restore-workspace",
            "abandoned-1",
            "retry-credentials-2",
        ] {
            let path = root.join(format!("{id}.{suffix}"));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("worker-copy"), b"duplicate output").unwrap();
        }
        fs::write(root.join(format!("{id}.unknown")), b"operator file").unwrap();
        let out = prune_peer(&c, python, 1);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for suffix in [
            "partial",
            "sealed",
            "credentials",
            "restore-workspace",
            "abandoned-1",
            "retry-credentials-2",
        ] {
            assert!(
                !root.join(format!("{id}.{suffix}")).exists(),
                "python={python}, suffix={suffix}"
            );
        }
        assert_eq!(
            (
                fs::read(&acceptance).unwrap(),
                fs::metadata(&acceptance).unwrap().ino(),
                fs::read(&marker).unwrap(),
                fs::metadata(&marker).unwrap().ino()
            ),
            retained
        );
        assert!(root.join(format!("{id}.intent.json")).is_file());
        assert!(root.join(format!("{id}.unknown")).is_file());
        assert!(prune_peer(&c, python, 1).status.success());
    }
}
#[test]
fn scheduled_pruning_rejects_contention_and_redirects_before_deletion_in_both_engines() {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let root = t.path().join("backups");
        application_backup::capture(&c, "timer-20260101T000000Z-1", false).unwrap();
        let lease = durable::lock(&root.join("lock"), true, false).unwrap();
        assert!(!prune_peer(&c, python, 1).status.success());
        assert!(
            root.join("timer-20260101T000000Z-1/acceptance.json")
                .is_file()
        );
        drop(lease);
        std::os::unix::fs::symlink(
            t.path().join("adapter.py"),
            root.join("timer-20260101T000001Z-1.partial"),
        )
        .unwrap();
        assert!(!prune_peer(&c, python, 1).status.success());
        assert!(t.path().join("adapter.py").is_file());
        assert!(
            root.join("timer-20260101T000000Z-1/acceptance.json")
                .is_file()
        );
    }
}

#[test]
fn interrupted_scheduled_retention_resumes_partial_deletion_without_rechecking_removed_acceptance_in_both_engines()
 {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let root = t.path().join("backups");
        for id in ["timer-20260101T000000Z-1", "timer-20260101T000001Z-1"] {
            application_backup::capture(&c, id, false).unwrap();
        }
        let latest = fs::read(root.join("LAST_SUCCESS")).unwrap();
        let tombstone = root.join("timer-20260101T000000Z-1.pruning");
        fs::rename(root.join("timer-20260101T000000Z-1"), &tombstone).unwrap();
        durable::sync_directory(&root).unwrap();
        fs::remove_file(tombstone.join("acceptance.json")).unwrap();
        // Fresh siblings must also retire immediately, rather than wait for the
        // incomplete grace once deletion has already been durably selected.
        fs::write(root.join("timer-20260101T000000Z-1.intent.json"), b"{}").unwrap();
        let output = prune_peer(&c, python, 1);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!tombstone.exists());
        assert!(!root.join("timer-20260101T000000Z-1.intent.json").exists());
        assert!(
            root.join("timer-20260101T000001Z-1/acceptance.json")
                .is_file()
        );
        assert_eq!(fs::read(root.join("LAST_SUCCESS")).unwrap(), latest);
        assert!(prune_peer(&c, python, 1).status.success());
    }
}

fn backup_peer(
    c: &Value,
    operation: &str,
    argument: &str,
    retry: bool,
) -> harbor_db::storage::Result<Value> {
    let script = r#"import json,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db import application_backup
if sys.argv[5]:
    original_digest=application_backup.digest
    patch.object(application_backup,'machine',side_effect=lambda: original_digest(sys.argv[5])).start()
c=json.loads(sys.argv[1])
try:
    if sys.argv[2]=='capture': result=application_backup.capture(c,sys.argv[3],retry_incomplete=sys.argv[4]=='true')
    else: result=application_backup.inspect(c,Path(sys.argv[3]))
    print(json.dumps({'ok':result}))
except ValueError as error: print(json.dumps({'error':str(error)}))
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        c.to_string(),
        operation.into(),
        argument.into(),
        retry.to_string(),
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
    let outcome: Value = serde_json::from_slice(&process::execute(&spec)?)?;
    if let Some(error) = outcome["error"].as_str() {
        return Err(harbor_db::storage::invalid(error));
    }
    Ok(outcome["ok"].clone())
}

#[test]
fn stale_capture_preserves_success_and_allows_the_same_attempt_to_retry_in_both_engines() {
    use std::os::unix::fs::MetadataExt;
    for python in [false, true] {
        let (t, mut c) = backup_fixture();
        c["maximum_age_seconds"] = json!(6);
        let capture = |attempt: &str, retry| {
            if python {
                backup_peer(&c, "capture", attempt, retry)
            } else {
                application_backup::capture(&c, attempt, retry)
            }
        };
        capture("previous", false).unwrap();
        let marker = t.path().join("backups/LAST_SUCCESS");
        let previous = fs::read(&marker).unwrap();
        let inode = fs::metadata(&marker).unwrap().ino();
        fs::write(t.path().join("slow_cleanup"), b"").unwrap();
        let error = capture("deterministic", false).unwrap_err();
        assert!(
            error.to_string().contains("stale"),
            "python={python}: {error}"
        );
        assert_eq!(fs::read(&marker).unwrap(), previous);
        assert_eq!(fs::metadata(&marker).unwrap().ino(), inode);
        assert!(!t.path().join("backups/deterministic").exists());
        assert!(t.path().join("backups/deterministic.partial").is_dir());
        assert!(
            !t.path()
                .join("backups/deterministic.restore-workspace")
                .exists()
        );
        fs::remove_file(t.path().join("slow_cleanup")).unwrap();
        capture("deterministic", true).unwrap();
        let backup = t.path().join("backups/deterministic");
        application_backup::verify_bytes(&c, &backup, None).unwrap();
        backup_peer(&c, "inspect", backup.to_str().unwrap(), false).unwrap();
        assert_eq!(last(t.path())["attempt"], "deterministic");
    }
}

#[test]
fn published_backup_files_are_read_only_in_both_engines() {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        if python {
            backup_peer(&c, "capture", "point", false).unwrap();
        } else {
            application_backup::capture(&c, "point", false).unwrap();
        }
        let backup = t.path().join("backups/point");
        let receipt = fs::read(backup.join("acceptance.json")).unwrap();
        for name in ["records.json", "capture.json", "acceptance.json"] {
            let path = backup.join(name);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o440,
                "python={python}: {name} remains writable"
            );
            if unsafe { libc::geteuid() } != 0 {
                assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
            }
        }
        application_backup::inspect(&c, &backup, None).unwrap();
        backup_peer(&c, "inspect", backup.to_str().unwrap(), false).unwrap();
        assert_eq!(fs::read(backup.join("acceptance.json")).unwrap(), receipt);
    }
}

#[test]
fn empty_directory_changes_invalidate_accepted_artifacts_in_both_engines() {
    for python in [false, true] {
        let (t, c) = backup_fixture();
        fs::write(t.path().join("empty_directory"), b"").unwrap();
        let receipt = if python {
            backup_peer(&c, "capture", "point", false).unwrap()
        } else {
            application_backup::capture(&c, "point", false).unwrap()
        };
        assert_eq!(
            receipt["artifacts"]["required-empty/nested"],
            json!({"directory":true})
        );
        let backup = t.path().join("backups/point");
        let inspect = || {
            if python {
                backup_peer(&c, "inspect", backup.to_str().unwrap(), false)
            } else {
                application_backup::verify_bytes(&c, &backup, None)
            }
        };
        inspect().unwrap();
        fs::remove_dir(backup.join("required-empty/nested")).unwrap();
        assert!(
            inspect().is_err(),
            "python={python}: empty directory removal accepted"
        );
        fs::create_dir(backup.join("required-empty/nested")).unwrap();
        inspect().unwrap();
        fs::rename(backup.join("required-empty"), backup.join("renamed-empty")).unwrap();
        assert!(inspect().is_err());
        fs::rename(backup.join("renamed-empty"), backup.join("required-empty")).unwrap();
        fs::remove_dir(backup.join("required-empty/nested")).unwrap();
        fs::write(backup.join("required-empty/nested"), b"").unwrap();
        assert!(
            inspect().is_err(),
            "python={python}: file/directory replacement accepted"
        );
    }
}

#[test]
fn hard_linked_capture_and_accepted_artifacts_reject_in_both_engines_without_chmod() {
    use std::os::unix::fs::MetadataExt;
    for python in [false, true] {
        let (t, c) = backup_fixture();
        let config_file = t.path().join("config.json");
        durable::write_json(&config_file, &c).unwrap();
        let invoke = |operation: &str, argument: &str| {
            if python {
                let script = "import json,sys\nfrom pathlib import Path\nfrom harbor_db import application_backup\nfrom unittest.mock import patch\nif sys.argv[4]:\n original_digest=application_backup.digest\n patch.object(application_backup,'machine',side_effect=lambda: original_digest(sys.argv[4])).start()\nc=json.loads(Path(sys.argv[1]).read_text())\ntry:\n if sys.argv[2]=='capture': application_backup.capture(c,sys.argv[3])\n else: application_backup.inspect(c,Path(sys.argv[3]))\n outcome={'ok':True}\nexcept ValueError as error: outcome={'error':str(error)}\nprint(json.dumps(outcome))";
                let mut spec = process::CommandSpec::new(vec![
                    "python3".into(),
                    "-B".into(),
                    "-c".into(),
                    script.into(),
                    config_file.display().to_string(),
                    operation.into(),
                    argument.into(),
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
                process::execute(&spec).and_then(|output| {
                    let outcome: Value = serde_json::from_slice(&output)?;
                    if let Some(error) = outcome["error"].as_str() {
                        Err(harbor_db::storage::invalid(error))
                    } else {
                        assert_eq!(outcome, json!({"ok":true}));
                        Ok(())
                    }
                })
            } else if operation == "capture" {
                application_backup::capture(&c, argument, false).map(|_| ())
            } else {
                application_backup::verify_bytes(&c, Path::new(argument), None).map(|_| ())
            }
        };
        invoke("capture", "first").unwrap_or_else(|error| panic!("python={python}: {error}"));
        let machine_path = option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or("/etc/machine-id");
        let accepted_receipt =
            durable::read_json(&t.path().join("backups/first/acceptance.json")).unwrap();
        assert_eq!(
            accepted_receipt["executor_machine_sha256"],
            codec::file_digest(&fs::canonicalize(machine_path).unwrap()).unwrap()
        );
        let marker = t.path().join("backups/LAST_SUCCESS");
        let previous = fs::read(&marker).unwrap();
        let marker_inode = fs::metadata(&marker).unwrap().ino();
        let external = t.path().join("mutable.json");
        fs::write(&external, br#"[{"id":1,"revision":7}]"#).unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(t.path().join("hardlink"), b"").unwrap();
        let error = invoke("capture", "linked").unwrap_err();
        assert!(
            error.to_string().contains("hard link"),
            "python={python}: {error}"
        );
        assert!(!t.path().join("backups/linked").exists());
        assert!(
            !t.path()
                .join("backups/linked.partial/acceptance.json")
                .exists()
        );
        assert!(!t.path().join("backups/linked.restore-workspace").exists());
        assert_eq!(fs::metadata(&external).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&external).unwrap(), br#"[{"id":1,"revision":7}]"#);
        assert_eq!(fs::read(&marker).unwrap(), previous);
        assert_eq!(fs::metadata(&marker).unwrap().ino(), marker_inode);
        let accepted = t.path().join("backups/first");
        invoke("inspect", accepted.to_str().unwrap()).unwrap();
        let artifact = accepted.join("records.json");
        fs::hard_link(&artifact, t.path().join("accepted-alias")).unwrap();
        let error = invoke("inspect", accepted.to_str().unwrap()).unwrap_err();
        assert!(
            error.to_string().contains("hard link"),
            "python={python}: {error}"
        );
        fs::remove_file(t.path().join("accepted-alias")).unwrap();
        invoke("inspect", accepted.to_str().unwrap()).unwrap();
    }
}

#[test]
fn last_success_syncs_reader_permissions_before_publication_in_both_engines() {
    use std::{
        os::unix::fs::MetadataExt,
        process::{Command, Stdio},
    };
    let injector = tempfile::tempdir().unwrap();
    let source = injector.path().join("marker-sync.c");
    let library = injector.path().join("marker-sync.so");
    fs::write(
        &source,
        r#"#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
int fsync(int fd) {
    int (*next)(int) = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    char link[64], path[4096];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(link, path, sizeof(path) - 1);
    if (length >= 0) {
        path[length] = '\0';
        const char *name = strrchr(path, '/');
        struct stat info;
        if (name && (strcmp(name + 1, "LAST_SUCCESS") == 0 ||
            strncmp(name + 1, ".LAST_SUCCESS.", 14) == 0) &&
            fstat(fd, &info) == 0 && S_ISREG(info.st_mode)) {
            if ((info.st_mode & 0777) != 0640 || getenv("HARBOR_DB_TEST_FAIL_MARKER_SYNC")) {
                errno = EIO;
                return -1;
            }
        }
    }
    return next(fd);
}
"#,
    )
    .unwrap();
    let compile = process::CommandSpec::new(vec![
        "cc".into(),
        "-shared".into(),
        "-fPIC".into(),
        "-Wall".into(),
        "-Wextra".into(),
        "-Werror".into(),
        source.to_str().unwrap().into(),
        "-o".into(),
        library.to_str().unwrap().into(),
        "-ldl".into(),
    ]);
    process::execute(&compile).unwrap();
    for native in [true, false] {
        let (temp, config) = backup_fixture();
        let config_path = temp.path().join("config.json");
        fs::write(&config_path, config.to_string()).unwrap();
        let run = |attempt: &str, fail: bool| {
            let mut command = if native {
                let mut command = Command::new(env!("CARGO_BIN_EXE_harbor-db-application-backup"));
                command.args([
                    "--config",
                    config_path.to_str().unwrap(),
                    "capture",
                    "--attempt",
                    attempt,
                ]);
                command
            } else {
                let mut command = Command::new("python3");
                command.args([
                    "-B",
                    "-c",
                    r#"import json,sys
from harbor_db import application_backup
from unittest.mock import patch
if sys.argv[3]:
    original=application_backup.digest
    patch.object(application_backup,'machine',side_effect=lambda: original(sys.argv[3])).start()
print(json.dumps(application_backup.capture(json.load(open(sys.argv[1])),sys.argv[2])))
"#,
                    config_path.to_str().unwrap(),
                    attempt,
                    option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or(""),
                ]);
                command
            };
            command
                .env("LD_PRELOAD", &library)
                .env(
                    "PYTHONPATH",
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                )
                .env_remove("HARBOR_DB_TEST_FAIL_MARKER_SYNC")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if fail {
                command.env("HARBOR_DB_TEST_FAIL_MARKER_SYNC", "1");
            }
            process::spawn(&mut command)
                .unwrap()
                .wait_with_output()
                .unwrap()
        };
        let accepted = run("good", false);
        assert!(
            accepted.status.success(),
            "native={native}: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        let marker = temp.path().join("backups/LAST_SUCCESS");
        let metadata = fs::metadata(&marker).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o640);
        let before = fs::read(&marker).unwrap();
        let rejected = run("failed-sync", true);
        assert!(
            !rejected.status.success(),
            "file-sync failure was acknowledged"
        );
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("Input/output error"));
        assert_eq!(fs::read(&marker).unwrap(), before);
        assert_eq!(fs::metadata(&marker).unwrap().ino(), metadata.ino());
        assert_eq!(last(temp.path())["attempt"], "good");
        assert!(
            temp.path()
                .join("backups/failed-sync/acceptance.json")
                .is_file()
        );
        assert!(
            fs::read_dir(temp.path().join("backups"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".LAST_SUCCESS."))
        );
    }
}

fn python_backup(c: &Value, action: &str) -> harbor_db::storage::Result<Value> {
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import application_backup
from unittest.mock import patch
if sys.argv[3]:
    original_digest=application_backup.digest
    patch.object(application_backup,'machine',side_effect=lambda: original_digest(sys.argv[3])).start()
config,action=json.loads(sys.argv[1]),sys.argv[2]
if action == 'inspect': result=application_backup.inspect(config,Path(config['root'])/'mixed')
else: result=application_backup.capture(config,'mixed',retry_incomplete=action=='retry')
print(json.dumps(result))
"#;
    let mut command = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        c.to_string(),
        action.into(),
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
    Ok(serde_json::from_slice(&process::execute(&command)?)?)
}

#[test]
fn python_and_rust_resume_each_others_failed_capture_and_verify_immutable_restore_points() {
    for python_first in [true, false] {
        let (t, c) = backup_fixture();
        fs::write(t.path().join("fail_restore"), b"").unwrap();
        if python_first {
            assert!(python_backup(&c, "capture").is_err());
        } else {
            assert!(application_backup::capture(&c, "mixed", false).is_err());
        }
        assert!(
            t.path()
                .join("backups/mixed.partial/records.json")
                .is_file()
        );
        assert!(!t.path().join("backups/LAST_SUCCESS").exists());
        let original_intent = fs::read(t.path().join("backups/mixed.intent.json")).unwrap();
        fs::remove_file(t.path().join("fail_restore")).unwrap();
        let resumed = if python_first {
            application_backup::capture(&c, "mixed", true).unwrap()
        } else {
            python_backup(&c, "retry").unwrap()
        };
        assert_eq!(resumed["status"], "verified");
        let point = t.path().join("backups/mixed");
        let acceptance = fs::read(point.join("acceptance.json")).unwrap();
        assert_eq!(
            application_backup::inspect(&c, &point, None).unwrap(),
            resumed
        );
        assert_eq!(python_backup(&c, "inspect").unwrap(), resumed);
        assert_eq!(fs::read(point.join("acceptance.json")).unwrap(), acceptance);
        assert_eq!(
            last(t.path())["acceptance_sha256"],
            codec::digest(&acceptance)
        );
        let abandoned = fs::read_dir(t.path().join("backups"))
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("mixed.abandoned-")
                    .then_some(entry.path())
            })
            .collect::<Vec<_>>();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(
            fs::read(abandoned[0].join("mixed.intent.json")).unwrap(),
            original_intent
        );
        assert!(abandoned[0].join("mixed.partial/records.json").is_file());
    }
}
#[test]
fn backup_success_preserves_prior_point_after_failure_and_detects_hash_age_and_tools() {
    let (t, c) = backup_fixture();
    let receipt = application_backup::capture(&c, "good", false).unwrap();
    assert_eq!(receipt["status"], "verified");
    assert_eq!(last(t.path())["attempt"], "good");
    let point = t.path().join("backups/good");
    application_backup::inspect(&c, &point, None).unwrap();
    assert!(application_backup::capture(&c, "good", false).is_err());
    fs::write(t.path().join("fail_restore"), b"").unwrap();
    let failure = application_backup::capture(&c, "failed", false).unwrap_err();
    assert_eq!(last(t.path())["attempt"], "good");
    assert!(
        t.path()
            .join("backups/failed.partial/records.json")
            .exists(),
        "failed capture did not retain records: {failure}"
    );
    assert!(!t.path().join("backups/failed.restore-workspace").exists());
    assert!(
        application_backup::inspect(&c, &point, Some(1_000_000_000_000))
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    assert!(application_backup::inspect(&c, &point, Some(0)).is_err());
    // Deliberately restore write permission to inject byte corruption into a
    // read-only accepted artifact; ordinary service-account writes are denied.
    fs::set_permissions(
        point.join("records.json"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    fs::write(point.join("records.json"), "[]").unwrap();
    assert!(
        application_backup::inspect(&c, &point, None)
            .unwrap_err()
            .to_string()
            .contains("hash")
    );
    fs::write(point.join("records.json"), "[{\"id\":1,\"revision\":7}]").unwrap();
    fs::write(
        t.path().join("adapter.py"),
        format!("{ADAPTER}\n# changed identity\n"),
    )
    .unwrap();
    assert!(
        application_backup::inspect(&c, &point, None)
            .unwrap_err()
            .to_string()
            .contains("executable identity")
    );
}
#[test]
fn redirected_mutating_or_incomplete_capture_cannot_publish() {
    for flag in [
        "symlink",
        "mutate",
        "mutate_cleanup",
        "self_accept",
        "incomplete_semantic",
        "wrong_semantic",
    ] {
        let (t, c) = backup_fixture();
        fs::write(t.path().join(flag), b"").unwrap();
        assert!(
            application_backup::capture(&c, "bad", false).is_err(),
            "{flag}"
        );
        assert!(!t.path().join("backups/LAST_SUCCESS").exists());
        assert!(!t.path().join("backups/bad").exists());
    }
}

#[test]
fn reserved_attempts_cannot_poison_backup_publication_in_both_engines() {
    use std::os::unix::fs::MetadataExt;
    for python in [false, true] {
        for populated in [false, true] {
            let (t, c) = backup_fixture();
            if populated {
                application_backup::capture(&c, "retained", false).unwrap();
            }
            let root = t.path().join("backups");
            let lock = root.join("lock");
            let info = lock.metadata().unwrap();
            let identity = (info.dev(), info.ino());
            let before: Vec<_> = fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            let marker = fs::read(root.join("LAST_SUCCESS")).ok();
            let config = t.path().join("config.json");
            durable::write_json(&config, &c).unwrap();
            for attempt in ["LAST_SUCCESS", "lock"] {
                for retry in [false, true] {
                    let mut command = if python {
                        let mut command = std::process::Command::new("python3");
                        command.args(["-B", "-m", "harbor_db.application_backup"]);
                        command.env(
                            "PYTHONPATH",
                            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                        );
                        command
                    } else {
                        std::process::Command::new(env!(
                            "CARGO_BIN_EXE_harbor-db-application-backup"
                        ))
                    };
                    command
                        .arg("--config")
                        .arg(&config)
                        .args(["capture", "--attempt", attempt]);
                    if retry {
                        command.arg("--retry-incomplete");
                    }
                    command
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped());
                    let result = process::spawn(&mut command)
                        .unwrap()
                        .wait_with_output()
                        .unwrap();
                    assert_eq!(result.status.code(), Some(1));
                    assert!(
                        String::from_utf8_lossy(&result.stderr).contains("invalid backup attempt"),
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    assert!(result.stdout.is_empty());
                    assert_eq!(fs::read(root.join("LAST_SUCCESS")).ok(), marker);
                    let after: Vec<_> = fs::read_dir(&root)
                        .unwrap()
                        .map(|entry| entry.unwrap().file_name())
                        .collect();
                    assert_eq!(before, after);
                    let info = lock.metadata().unwrap();
                    assert_eq!(identity, (info.dev(), info.ino()));
                }
            }
            let receipt = if python {
                backup_peer(&c, "capture", "valid-next", false).unwrap()
            } else {
                application_backup::capture(&c, "valid-next", false).unwrap()
            };
            assert_eq!(receipt["attempt"], "valid-next");
            assert_eq!(last(t.path())["attempt"], "valid-next");
        }
    }
}

#[test]
fn artifact_inventory_rejects_non_utf8_files_and_directories_without_collapsing_valid_names() {
    use std::os::unix::ffi::OsStringExt;
    let t = tempfile::tempdir().unwrap();
    let names = ["artifact-\u{fffd}", "artifact-é", "artifact-e\u{301}"];
    for (index, name) in names.iter().enumerate() {
        fs::write(t.path().join(name), index.to_string()).unwrap();
    }
    let expected = application_backup::inventory(t.path()).unwrap();
    assert_eq!(expected.as_object().unwrap().len(), names.len());
    for (index, name) in names.iter().enumerate() {
        assert_eq!(expected[*name], codec::digest(index.to_string().as_bytes()));
    }
    for directory in [false, true] {
        let mut invalid_paths = Vec::new();
        for byte in [0xff, 0xfe] {
            let path = t
                .path()
                .join(std::ffi::OsString::from_vec(vec![b'a', byte]));
            if directory {
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, [byte]).unwrap();
            }
            invalid_paths.push(path);
        }
        let error = application_backup::inventory(t.path()).unwrap_err();
        assert!(error.to_string().contains("UTF-8"), "{error}");
        for path in invalid_paths {
            if directory {
                fs::remove_dir(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
        assert_eq!(application_backup::inventory(t.path()).unwrap(), expected);
    }
}

#[test]
fn lossy_capture_paths_cannot_publish_or_replace_the_previous_restore_point_in_either_engine() {
    for native in [true, false] {
        let (t, c) = backup_fixture();
        application_backup::capture(&c, "good", false).unwrap();
        let previous = fs::read(t.path().join("backups/LAST_SUCCESS")).unwrap();
        let acceptance = fs::read(t.path().join("backups/good/acceptance.json")).unwrap();
        fs::write(t.path().join("non_utf8"), b"").unwrap();
        let result = if native {
            application_backup::capture(&c, "mixed", false)
        } else {
            python_backup(&c, "capture")
        };
        assert!(
            result.is_err(),
            "non-UTF-8 capture was accepted: {result:?}"
        );
        assert!(!t.path().join("backups/mixed").exists());
        assert!(
            !t.path()
                .join("backups/mixed.partial/acceptance.json")
                .exists()
        );
        assert!(!t.path().join("backups/mixed.restore-workspace").exists());
        assert_eq!(
            fs::read(t.path().join("backups/LAST_SUCCESS")).unwrap(),
            previous
        );
        assert_eq!(
            fs::read(t.path().join("backups/good/acceptance.json")).unwrap(),
            acceptance
        );
        application_backup::inspect(&c, &t.path().join("backups/good"), None).unwrap();
    }
}
#[test]
fn missing_anchor_is_never_recreated_and_cleanup_failure_retains_private_workspace() {
    let (t, c) = backup_fixture();
    application_backup::capture(&c, "good", false).unwrap();
    fs::write(t.path().join("fail_cleanup"), b"").unwrap();
    assert!(application_backup::capture(&c, "incomplete", false).is_err());
    assert_eq!(
        fs::metadata(t.path().join("backups/incomplete.restore-workspace"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(last(t.path())["attempt"], "good");
    fs::remove_file(t.path().join("backups/lock")).unwrap();
    assert!(application_backup::capture(&c, "another", false).is_err());
    assert!(!t.path().join("backups/lock").exists());
}
#[test]
fn interrupted_capture_resumes_only_immutable_intent_and_retains_evidence() {
    let (t, c) = backup_fixture();
    fs::write(t.path().join("fail_restore"), b"").unwrap();
    assert!(application_backup::capture(&c, "interrupted", false).is_err());
    let mut changed = c.clone();
    changed["maximum_age_seconds"] = json!(7200);
    let rejection = application_backup::capture(&changed, "interrupted", true).unwrap_err();
    assert!(
        rejection.to_string().contains("intent changed"),
        "unexpected retry rejection: {rejection}"
    );
    fs::remove_file(t.path().join("fail_restore")).unwrap();
    application_backup::capture(&c, "interrupted", true).unwrap();
    assert_eq!(last(t.path())["attempt"], "interrupted");
    let retained: Vec<_> = fs::read_dir(t.path().join("backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("interrupted.abandoned-")
        })
        .collect();
    assert_eq!(retained.len(), 1);
    assert!(
        retained[0]
            .join("interrupted.partial/records.json")
            .exists()
    );
}
#[test]
fn backup_children_retain_coordinator_and_inherited_leases() {
    let (t, c) = backup_fixture();
    let extra = t.path().join("extra.lock");
    fs::write(&extra, b"").unwrap();
    let lease = durable::lock(&extra, true, false).unwrap();
    let manifest = t.path().join("config.json");
    durable::write_json(&manifest, &c).unwrap();
    let mut spec = process::CommandSpec::new(vec![
        env!("CARGO_BIN_EXE_harbor-db-application-backup").into(),
        "--config".into(),
        manifest.display().to_string(),
        "capture".into(),
        "--attempt".into(),
        "inherited".into(),
    ]);
    let mut environment: std::collections::BTreeMap<_, _> = std::env::vars().collect();
    environment.insert("HARBOR_DB_LEASE_FDS".into(), lease.fd().to_string());
    environment.insert(
        "HARBOR_DB_TEST_MACHINE_ID".into(),
        t.path().join("absent-machine-id").display().to_string(),
    );
    spec.environment = Some(environment);
    spec.leases = vec![lease.fd()];
    process::execute(&spec).unwrap();
    assert_eq!(last(t.path())["attempt"], "inherited");
    let machine_path = if cfg!(feature = "testing") {
        option_env!("HARBOR_DB_TEST_MACHINE_ID").unwrap_or("/etc/machine-id")
    } else {
        "/etc/machine-id"
    };
    let receipt = durable::read_json(&t.path().join("backups/inherited/acceptance.json")).unwrap();
    assert_eq!(
        receipt["executor_machine_sha256"],
        codec::file_digest(&fs::canonicalize(machine_path).unwrap()).unwrap(),
        "runtime environment must not override executor custody"
    );
    let mut environment = spec.environment.take().unwrap();
    environment.insert("HARBOR_DB_LEASE_FDS".into(), "2".into());
    spec.environment = Some(environment);
    spec.argv
        .last_mut()
        .unwrap()
        .clone_from(&"invalid".to_owned());
    assert!(process::execute(&spec).is_err());
    assert!(!t.path().join("backups/invalid").exists());
}
#[test]
fn independent_certification_checks_host_privacy_hashes_and_retains_receipts() {
    let (t, c) = backup_fixture();
    application_backup::capture(&c, "good", false).unwrap();
    let b = t.path().join("backups/good");
    let state = t.path().join("certifier");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(state.join("lock"), b"").unwrap();
    assert!(
        application_backup::certify(&c, &b, &state)
            .unwrap_err()
            .to_string()
            .contains("different machine")
    );
    let acceptance = b.join("acceptance.json");
    let mut source = durable::read_json(&acceptance).unwrap();
    source["executor"] = json!("foreign-host");
    source["executor_machine_sha256"] = json!("0".repeat(64));
    durable::write_json(&acceptance, &source).unwrap();
    // Foreign-host metadata exercises certification locally; true two-host execution is a VM qualification gate.
    let result = application_backup::certify(&c, &b, &state).unwrap();
    assert_eq!(result["status"], "verified");
    let hash = codec::file_digest(&acceptance).unwrap();
    assert_eq!(result["source_acceptance_sha256"], hash);
    let path = state.join(format!("{hash}.json"));
    assert_eq!(durable::read_json(&path).unwrap(), result);
    durable::write_json(&path, &json!({"original":"retain"})).unwrap();
    assert!(
        application_backup::certify(&c, &b, &state)
            .unwrap_err()
            .to_string()
            .contains("retain its original evidence")
    );
    assert_eq!(
        durable::read_json(&path).unwrap(),
        json!({"original":"retain"})
    );
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(application_backup::certify(&c, &b, &state).is_err());
}

fn certify_peer(c: &Value, backup: &Path, state: &Path) -> harbor_db::storage::Result<Value> {
    let script = r#"import json,sys
from unittest.mock import patch
from harbor_db import application_backup
if sys.argv[4]:
    original_digest=application_backup.digest
    patch.object(application_backup,'machine',side_effect=lambda: original_digest(sys.argv[4])).start()
try: print(json.dumps({'ok':application_backup.certify(json.loads(sys.argv[1]),sys.argv[2],sys.argv[3])}))
except ValueError as error: print(json.dumps({'error':str(error)}))
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        c.to_string(),
        backup.to_string_lossy().into_owned(),
        state.to_string_lossy().into_owned(),
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
    let response: Value = serde_json::from_slice(&process::execute(&spec)?).unwrap();
    if let Some(error) = response["error"].as_str() {
        return Err(harbor_db::storage::StorageError::Invalid(error.into()));
    }
    Ok(response["ok"].clone())
}

#[test]
fn independent_certification_retries_preserve_original_evidence_across_engines_and_later_seconds() {
    use std::os::unix::fs::MetadataExt;
    for python_first in [false, true] {
        let (t, c) = backup_fixture();
        application_backup::capture(&c, "good", false).unwrap();
        let backup = t.path().join("backups/good");
        let acceptance = backup.join("acceptance.json");
        let mut source = durable::read_json(&acceptance).unwrap();
        source["executor"] = json!("foreign-host");
        source["executor_machine_sha256"] = json!("0".repeat(64));
        durable::write_json(&acceptance, &source).unwrap();
        let state = t.path().join("certifier");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(state.join("lock"), b"").unwrap();
        let original = if python_first {
            certify_peer(&c, &backup, &state).unwrap()
        } else {
            application_backup::certify(&c, &backup, &state).unwrap()
        };
        let path = state.join(format!("{}.json", codec::file_digest(&acceptance).unwrap()));
        let bytes = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(chrono::Utc::now().timestamp() > original["certified_at"].as_i64().unwrap());
        assert_eq!(
            application_backup::certify(&c, &backup, &state).unwrap(),
            original
        );
        assert_eq!(certify_peer(&c, &backup, &state).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        for (key, bad) in [
            ("certified_at", json!(chrono::Utc::now().timestamp() + 3600)),
            ("semantic_sha256", json!("f".repeat(64))),
        ] {
            let mut invalid = original.clone();
            invalid[key] = bad;
            durable::write_json(&path, &invalid).unwrap();
            assert!(application_backup::certify(&c, &backup, &state).is_err());
            assert!(certify_peer(&c, &backup, &state).is_err());
            assert_eq!(durable::read_json(&path).unwrap(), invalid);
        }
        for invalid in [json!(null), json!([]), json!(true), json!("unbound")] {
            durable::write_json(&path, &invalid).unwrap();
            assert!(application_backup::certify(&c, &backup, &state).is_err());
            assert!(certify_peer(&c, &backup, &state).is_err());
            assert_eq!(durable::read_json(&path).unwrap(), invalid);
        }
    }
}
#[test]
fn provisioning_validation() {
    let mut p = json!({"database":"app","owner_role":"owner","runtime_role":"runtime"});
    assert_eq!(provision::validate(&p).unwrap()["schema"], "public");
    p["runtime_role"] = json!("postgres");
    assert!(provision::validate(&p).is_err());
}
#[test]
fn provisioning_native_privilege_convergence_and_read_only_drift() {
    use process::{CommandSpec, Identity};
    use std::time::Duration;
    let package = PathBuf::from(
        std::env::var("HARBOR_DB_TEST_POSTGRES").expect("real PostgreSQL package is required"),
    );
    if std::env::var_os("HARBOR_DB_PROVISION_CHILD").is_none() {
        let root = tempfile::tempdir_in(
            std::env::var_os("HARBOR_DB_TEST_TMPDIR")
                .unwrap_or_else(|| "/data/scratch/tmp/opencode".into()),
        )
        .unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let identity = if unsafe { libc::getuid() } == 0 {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            let p = CString::new(root.path().as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chown(p.as_ptr(), 65534, 65534) }, 0);
            Some(Identity {
                uid: 65534,
                gid: 65534,
                groups: vec![65534],
            })
        } else {
            None
        };
        let mut spec = CommandSpec::new(vec![
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "provisioning_native_privilege_convergence_and_read_only_drift".into(),
            "--show-output".into(),
        ]);
        let mut env: std::collections::BTreeMap<_, _> = std::env::vars().collect();
        env.insert(
            "HARBOR_DB_PROVISION_CHILD".into(),
            root.path().display().to_string(),
        );
        spec.environment = Some(env);
        spec.identity = identity;
        // QEMU user-mode runs every real PostgreSQL client/server executable;
        // retain a finite platform-specific budget for the full convergence run.
        spec.timeout = Duration::from_secs(if cfg!(target_arch = "aarch64") {
            900
        } else {
            120
        });
        let output = process::run(&spec).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var("HARBOR_DB_PROVISION_CHILD").unwrap());
    let socket = root.join("socket");
    fs::create_dir(&socket).unwrap();
    let data = root.join("data");
    let run = |name: &str, args: Vec<String>| {
        let mut spec = CommandSpec::new(
            std::iter::once(package.join("bin").join(name).display().to_string())
                .chain(args)
                .collect(),
        );
        spec.timeout = Duration::from_secs(30);
        let env = std::env::vars()
            .filter(|(k, _)| !k.starts_with("PG"))
            .collect();
        spec.environment = Some(env);
        process::run(&spec).unwrap()
    };
    let out = run(
        "initdb",
        vec![
            "-D".into(),
            data.display().to_string(),
            "-U".into(),
            "control".into(),
            "-A".into(),
            "trust".into(),
            "--locale=C".into(),
            "--encoding=UTF8".into(),
        ],
    );
    assert!(out.status.success());
    let out = run(
        "pg_ctl",
        vec![
            "-D".into(),
            data.display().to_string(),
            "-l".into(),
            root.join("postgres.log").display().to_string(),
            "-o".into(),
            format!("-k {} -c listen_addresses= -p 55441", socket.display()),
            "-w".into(),
            "start".into(),
        ],
    );
    assert!(
        out.status.success(),
        "PostgreSQL fixture startup failed: {}\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        fs::read_to_string(root.join("postgres.log")).unwrap_or_default()
    );
    struct Stop {
        package: PathBuf,
        data: PathBuf,
    }
    impl Drop for Stop {
        fn drop(&mut self) {
            let mut spec = CommandSpec::new(vec![
                self.package.join("bin/pg_ctl").display().to_string(),
                "-D".into(),
                self.data.display().to_string(),
                "-m".into(),
                "fast".into(),
                "-w".into(),
                "stop".into(),
            ]);
            spec.timeout = Duration::from_secs(30);
            let _ = process::run(&spec);
        }
    }
    let _stop = Stop {
        package: package.clone(),
        data,
    };
    let lock = root.join("provision.lock");
    fs::write(&lock, b"").unwrap();
    let endpoint = json!({"package":package,"socket_dir":socket,"port":55441,"control_role":"control","lock_file":lock});
    let policy = json!({"database":"demo","owner_role":"demo_owner","runtime_role":"demo_runtime","schema":"public","table_privileges":["SELECT"],"sequence_privileges":["USAGE","SELECT"],"tables":{"documents":["SELECT","INSERT","UPDATE","DELETE"],"history":["SELECT","INSERT"]}});
    let python_provision = |policy: &Value, action: &str| {
        let mut spec = CommandSpec::new(vec![
            "python3".into(), "-B".into(), "-c".into(),
            "import json,sys; from harbor_db import provision; policy,endpoint=json.loads(sys.argv[1]),json.loads(sys.argv[2]); action=sys.argv[3]; print(json.dumps(provision.check(policy,endpoint))) if action=='check' else getattr(provision,action)(policy,endpoint)".into(),
            policy.to_string(), endpoint.to_string(), action.into(),
        ]);
        let mut environment: std::collections::BTreeMap<_, _> = std::env::vars().collect();
        environment.insert(
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        );
        spec.environment = Some(environment);
        process::run(&spec).unwrap()
    };
    let sql = |statement: &str, role: &str, db: &str| {
        run(
            "psql",
            vec![
                "-X".into(),
                "-w".into(),
                "-At".into(),
                "-v".into(),
                "ON_ERROR_STOP=1".into(),
                "-h".into(),
                socket.display().to_string(),
                "-p".into(),
                "55441".into(),
                "-U".into(),
                role.into(),
                "-d".into(),
                db.into(),
                "-c".into(),
                statement.into(),
            ],
        )
    };
    let success = |statement: &str, role: &str, db: &str| {
        let out = sql(statement, role, db);
        assert!(out.status.success(), "SQL failed: {statement}");
        process::text(&out.stdout).unwrap().trim().to_owned()
    };
    provision::apply(&policy, &endpoint).unwrap();
    provision::apply(&policy, &endpoint).unwrap();
    assert!(!provision::check(&policy, &endpoint).unwrap());
    assert!(python_provision(&policy, "apply").status.success());
    assert_eq!(
        process::text(&python_provision(&policy, "check").stdout)
            .unwrap()
            .trim(),
        "false"
    );
    success(
        "CREATE TABLE documents(id int);CREATE TABLE history(id int);CREATE TABLE immutable(id int);CREATE SEQUENCE cursor;CREATE FUNCTION hidden_write() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'INSERT INTO immutable VALUES(86) RETURNING id';CREATE PROCEDURE hidden_procedure() LANGUAGE sql AS 'SELECT 1';",
        "demo_owner",
        "demo",
    );
    provision::reconcile(&policy, &endpoint).unwrap();
    assert!(provision::check(&policy, &endpoint).unwrap());
    assert!(python_provision(&policy, "reconcile").status.success());
    for statement in ["SELECT hidden_write()", "CALL hidden_procedure()"] {
        assert!(
            !sql(statement, "demo_runtime", "demo").status.success(),
            "runtime executed undeclared routine: {statement}"
        );
    }
    assert_eq!(
        success("SELECT count(*) FROM immutable", "control", "demo"),
        "0"
    );
    assert_eq!(success("SELECT hidden_write()", "demo_owner", "demo"), "86");
    success(
        "CREATE FUNCTION future_function() RETURNS int LANGUAGE sql AS 'SELECT 87'",
        "demo_owner",
        "demo",
    );
    assert!(
        !sql("SELECT future_function()", "demo_runtime", "demo")
            .status
            .success()
    );
    let mut missing = policy.clone();
    missing["tables"]["missing_history"] = json!(["SELECT", "INSERT"]);
    let grants = success(
        "SELECT json_agg(json_build_array(relname,relacl::text) ORDER BY relname) FROM pg_class WHERE relnamespace='public'::regnamespace",
        "control",
        "demo",
    );
    assert!(!provision::check(&missing, &endpoint).unwrap());
    assert!(!provision::check(&missing, &endpoint).unwrap());
    assert!(provision::reconcile(&missing, &endpoint).is_err());
    assert_eq!(
        process::text(&python_provision(&missing, "check").stdout)
            .unwrap()
            .trim(),
        "false"
    );
    assert!(!python_provision(&missing, "reconcile").status.success());
    assert_eq!(
        success(
            "SELECT json_agg(json_build_array(relname,relacl::text) ORDER BY relname) FROM pg_class WHERE relnamespace='public'::regnamespace",
            "control",
            "demo",
        ),
        grants,
        "a missing declared table must not publish partially reconciled privileges"
    );
    success(
        "INSERT INTO documents VALUES(1);UPDATE documents SET id=2;DELETE FROM documents;INSERT INTO history VALUES(1);SELECT nextval('cursor');",
        "demo_runtime",
        "demo",
    );
    for statement in [
        "CREATE TABLE unauthorized(id int)",
        "UPDATE history SET id=2",
        "DELETE FROM history",
        "INSERT INTO immutable VALUES(1)",
        "SET ROLE demo_owner",
        "CREATE ROLE unauthorized",
    ] {
        assert!(
            !sql(statement, "demo_runtime", "demo").status.success(),
            "{statement}"
        );
    }
    success("CREATE TABLE future(id int)", "demo_owner", "demo");
    success("SELECT * FROM future", "demo_runtime", "demo");
    assert!(
        !sql("INSERT INTO future VALUES(1)", "demo_runtime", "demo")
            .status
            .success()
    );
    for statement in [
        "GRANT EXECUTE ON FUNCTION hidden_write() TO PUBLIC",
        "GRANT EXECUTE ON FUNCTION hidden_write() TO demo_runtime",
        "GRANT EXECUTE ON PROCEDURE hidden_procedure() TO PUBLIC",
        "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner GRANT EXECUTE ON FUNCTIONS TO PUBLIC",
        "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO demo_runtime",
        "GRANT UPDATE ON history TO demo_runtime",
        "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public GRANT UPDATE ON TABLES TO demo_runtime",
        "GRANT UPDATE(id) ON history TO demo_runtime",
        "GRANT SELECT ON history TO demo_runtime WITH GRANT OPTION",
        "GRANT MAINTAIN ON history TO demo_runtime",
        "GRANT CREATE ON DATABASE demo TO demo_runtime",
    ] {
        success(statement, "control", "demo");
        assert!(
            !provision::check(&policy, &endpoint).unwrap(),
            "{statement}"
        );
        assert!(
            !provision::check(&policy, &endpoint).unwrap(),
            "read-only check repaired {statement}"
        );
        provision::apply(&policy, &endpoint).unwrap();
        assert!(provision::check(&policy, &endpoint).unwrap());
        assert_eq!(
            process::text(&python_provision(&policy, "check").stdout)
                .unwrap()
                .trim(),
            "true"
        );
    }
    success("CREATE ROLE acl_outsider LOGIN", "control", "postgres");
    for (grant, revoke) in [
        (
            "GRANT EXECUTE ON FUNCTION hidden_write() TO acl_outsider",
            "REVOKE EXECUTE ON FUNCTION hidden_write() FROM acl_outsider",
        ),
        (
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO acl_outsider",
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public REVOKE EXECUTE ON FUNCTIONS FROM acl_outsider",
        ),
        (
            "GRANT SELECT ON documents TO acl_outsider",
            "REVOKE SELECT ON documents FROM acl_outsider",
        ),
        (
            "GRANT USAGE ON SEQUENCE cursor TO acl_outsider",
            "REVOKE USAGE ON SEQUENCE cursor FROM acl_outsider",
        ),
        (
            "GRANT SELECT(id) ON documents TO acl_outsider",
            "REVOKE SELECT(id) ON documents FROM acl_outsider",
        ),
        (
            "GRANT CONNECT ON DATABASE demo TO acl_outsider",
            "REVOKE CONNECT ON DATABASE demo FROM acl_outsider",
        ),
        (
            "GRANT USAGE ON SCHEMA public TO acl_outsider",
            "REVOKE USAGE ON SCHEMA public FROM acl_outsider",
        ),
        (
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner GRANT SELECT ON TABLES TO acl_outsider",
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner REVOKE SELECT ON TABLES FROM acl_outsider",
        ),
        (
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public GRANT USAGE ON SEQUENCES TO acl_outsider",
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public REVOKE USAGE ON SEQUENCES FROM acl_outsider",
        ),
    ] {
        success(grant, "control", "demo");
        assert!(!provision::check(&policy, &endpoint).unwrap(), "{grant}");
        assert_eq!(
            process::text(&python_provision(&policy, "check").stdout)
                .unwrap()
                .trim(),
            "false",
            "{grant}"
        );
        assert!(provision::reconcile(&policy, &endpoint).is_err(), "{grant}");
        assert!(
            !python_provision(&policy, "reconcile").status.success(),
            "{grant}"
        );
        success(revoke, "control", "demo");
        provision::reconcile(&policy, &endpoint).unwrap();
        assert!(python_provision(&policy, "reconcile").status.success());
        assert!(provision::check(&policy, &endpoint).unwrap());
    }
    success("DROP ROLE acl_outsider", "control", "postgres");
    for python in [false, true] {
        let prefix = if python {
            "peer_custom"
        } else {
            "native_custom"
        };
        let custom = json!({"database":prefix,"owner_role":format!("{prefix}_owner"),"runtime_role":format!("{prefix}_runtime"),"schema":"managed"});
        if python {
            assert!(python_provision(&custom, "apply").status.success());
        } else {
            provision::apply(&custom, &endpoint).unwrap();
        }
        assert!(provision::check(&custom, &endpoint).unwrap());
        let peer = python_provision(&custom, "check");
        assert!(peer.status.success() && process::text(&peer.stdout).unwrap().trim() == "true");
        success(&format!("DROP DATABASE {prefix}"), "control", "postgres");
        success(
            &format!("DROP ROLE {prefix}_runtime;DROP ROLE {prefix}_owner"),
            "control",
            "postgres",
        );
    }
    success(
        "CREATE SCHEMA unmanaged AUTHORIZATION demo_owner",
        "control",
        "demo",
    );
    success(
        "CREATE TABLE unmanaged.records(id int); CREATE SEQUENCE unmanaged.cursor; CREATE FUNCTION unmanaged.read_record() RETURNS int LANGUAGE sql AS 'SELECT 1'; REVOKE ALL ON FUNCTION unmanaged.read_record() FROM PUBLIC; CREATE TYPE unmanaged.record_kind AS ENUM ('one')",
        "control",
        "demo",
    );
    let mut grant_outcomes = Vec::new();
    for (grant, revoke) in [
        (
            "GRANT EXECUTE ON FUNCTION unmanaged.read_record() TO PUBLIC",
            "REVOKE EXECUTE ON FUNCTION unmanaged.read_record() FROM PUBLIC",
        ),
        (
            "GRANT USAGE ON SCHEMA unmanaged TO demo_runtime",
            "REVOKE USAGE ON SCHEMA unmanaged FROM demo_runtime",
        ),
        (
            "GRANT USAGE ON SCHEMA unmanaged TO PUBLIC",
            "REVOKE USAGE ON SCHEMA unmanaged FROM PUBLIC",
        ),
        (
            "GRANT USAGE ON SCHEMA unmanaged TO PUBLIC;GRANT UPDATE ON unmanaged.records TO PUBLIC",
            "REVOKE UPDATE ON unmanaged.records FROM PUBLIC;REVOKE USAGE ON SCHEMA unmanaged FROM PUBLIC",
        ),
        (
            "GRANT UPDATE ON unmanaged.records TO PUBLIC",
            "REVOKE UPDATE ON unmanaged.records FROM PUBLIC",
        ),
        (
            "GRANT UPDATE(id) ON unmanaged.records TO PUBLIC",
            "REVOKE UPDATE(id) ON unmanaged.records FROM PUBLIC",
        ),
        (
            "GRANT USAGE ON SEQUENCE unmanaged.cursor TO PUBLIC",
            "REVOKE USAGE ON SEQUENCE unmanaged.cursor FROM PUBLIC",
        ),
        (
            "GRANT UPDATE ON unmanaged.records TO demo_runtime",
            "REVOKE UPDATE ON unmanaged.records FROM demo_runtime",
        ),
        (
            "GRANT UPDATE(id) ON unmanaged.records TO demo_runtime",
            "REVOKE UPDATE(id) ON unmanaged.records FROM demo_runtime",
        ),
        (
            "GRANT USAGE ON SEQUENCE unmanaged.cursor TO demo_runtime",
            "REVOKE USAGE ON SEQUENCE unmanaged.cursor FROM demo_runtime",
        ),
        (
            "GRANT EXECUTE ON FUNCTION unmanaged.read_record() TO demo_runtime",
            "REVOKE EXECUTE ON FUNCTION unmanaged.read_record() FROM demo_runtime",
        ),
        (
            "GRANT USAGE ON TYPE unmanaged.record_kind TO demo_runtime",
            "REVOKE USAGE ON TYPE unmanaged.record_kind FROM demo_runtime",
        ),
        (
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA unmanaged GRANT SELECT ON TABLES TO demo_runtime",
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA unmanaged REVOKE SELECT ON TABLES FROM demo_runtime",
        ),
    ] {
        success(grant, "control", "demo");
        let native_check = provision::check(&policy, &endpoint).unwrap();
        let python_check = python_provision(&policy, "check");
        assert!(
            python_check.status.success(),
            "{}",
            String::from_utf8_lossy(&python_check.stderr)
        );
        let python_check = process::text(&python_check.stdout).unwrap().trim() == "true";
        for python in [false, true] {
            for action in ["apply", "reconcile"] {
                // Deliberately stale managed ACLs must remain untouched when an
                // undeclared grant requires explicit operator adoption/removal.
                success(
                    "GRANT UPDATE ON immutable TO demo_runtime",
                    "control",
                    "demo",
                );
                let before = success(
                    "SELECT relacl::text FROM pg_class WHERE oid='public.immutable'::regclass",
                    "control",
                    "demo",
                );
                let rejected = if python {
                    !python_provision(&policy, action).status.success()
                } else if action == "apply" {
                    provision::apply(&policy, &endpoint).is_err()
                } else {
                    provision::reconcile(&policy, &endpoint).is_err()
                };
                let after = success(
                    "SELECT relacl::text FROM pg_class WHERE oid='public.immutable'::regclass",
                    "control",
                    "demo",
                );
                grant_outcomes.push((
                    grant,
                    python,
                    action,
                    !native_check,
                    !python_check,
                    rejected,
                    before == after,
                ));
                success(
                    "REVOKE UPDATE ON immutable FROM demo_runtime",
                    "control",
                    "demo",
                );
            }
        }
        success(revoke, "control", "demo");
        provision::reconcile(&policy, &endpoint).unwrap();
        assert!(python_provision(&policy, "reconcile").status.success());
        assert!(provision::check(&policy, &endpoint).unwrap());
    }
    success(
        "DROP TABLE unmanaged.records;DROP SEQUENCE unmanaged.cursor;DROP FUNCTION unmanaged.read_record();DROP TYPE unmanaged.record_kind",
        "control",
        "demo",
    );
    assert!(
        grant_outcomes
            .iter()
            .all(|(_, _, _, a, b, c, d)| *a && *b && *c && *d),
        "{grant_outcomes:?}"
    );
    let mut ownership_outcomes = Vec::new();
    for (create, drop) in [
        (
            "CREATE TABLE unmanaged.outside(id int); ALTER TABLE unmanaged.outside OWNER TO demo_runtime",
            "DROP TABLE unmanaged.outside",
        ),
        (
            "CREATE SEQUENCE unmanaged.outside; ALTER SEQUENCE unmanaged.outside OWNER TO demo_runtime",
            "DROP SEQUENCE unmanaged.outside",
        ),
        (
            "CREATE VIEW unmanaged.outside AS SELECT 1; ALTER VIEW unmanaged.outside OWNER TO demo_runtime",
            "DROP VIEW unmanaged.outside",
        ),
        (
            "CREATE TYPE unmanaged.outside AS ENUM ('one'); ALTER TYPE unmanaged.outside OWNER TO demo_runtime",
            "DROP TYPE unmanaged.outside",
        ),
        (
            "CREATE FUNCTION unmanaged.outside() RETURNS int LANGUAGE sql AS 'SELECT 1'; ALTER FUNCTION unmanaged.outside() OWNER TO demo_runtime",
            "DROP FUNCTION unmanaged.outside()",
        ),
        (
            "CREATE SCHEMA runtime_owned AUTHORIZATION demo_runtime",
            "DROP SCHEMA runtime_owned",
        ),
    ] {
        success(create, "control", "demo");
        let before = success(
            "SELECT json_agg(json_build_array(relname,relacl::text) ORDER BY relname) FROM pg_class WHERE relnamespace='public'::regnamespace",
            "control",
            "demo",
        );
        let native_check = provision::check(&policy, &endpoint).unwrap();
        let python_check = python_provision(&policy, "check");
        assert!(
            python_check.status.success(),
            "{}",
            String::from_utf8_lossy(&python_check.stderr)
        );
        let python_check = process::text(&python_check.stdout).unwrap().trim() == "true";
        let native_rejected = provision::apply(&policy, &endpoint).is_err();
        let python_rejected = !python_provision(&policy, "apply").status.success();
        let after = success(
            "SELECT json_agg(json_build_array(relname,relacl::text) ORDER BY relname) FROM pg_class WHERE relnamespace='public'::regnamespace",
            "control",
            "demo",
        );
        ownership_outcomes.push((
            create,
            !native_check,
            !python_check,
            native_rejected,
            python_rejected,
            before == after,
        ));
        success(drop, "control", "demo");
        assert!(provision::check(&policy, &endpoint).unwrap());
    }
    success("DROP SCHEMA unmanaged", "control", "demo");
    assert!(
        ownership_outcomes
            .iter()
            .all(|(_, a, b, c, d, e)| *a && *b && *c && *d && *e),
        "{ownership_outcomes:?}"
    );
    success("CREATE DATABASE foreign_database", "control", "postgres");
    let mut foreign = policy.clone();
    foreign["database"] = json!("foreign_database");
    assert!(
        provision::apply(&foreign, &endpoint)
            .unwrap_err()
            .to_string()
            .contains("ownership")
    );
    assert_eq!(
        success(
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname='foreign_database'",
            "control",
            "postgres"
        ),
        "control"
    );
    success("DROP DATABASE foreign_database", "control", "postgres");
    success("CREATE SCHEMA foreign_schema", "control", "demo");
    foreign = policy.clone();
    foreign["schema"] = json!("foreign_schema");
    assert!(
        provision::apply(&foreign, &endpoint)
            .unwrap_err()
            .to_string()
            .contains("ownership")
    );
    assert_eq!(
        success(
            "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='foreign_schema'",
            "control",
            "demo"
        ),
        "control"
    );
    success("DROP SCHEMA foreign_schema", "control", "demo");
    success(
        "CREATE ROLE outsider LOGIN;GRANT demo_owner TO outsider",
        "control",
        "postgres",
    );
    assert!(!provision::check(&policy, &endpoint).unwrap());
    assert!(
        provision::apply(&policy, &endpoint)
            .unwrap_err()
            .to_string()
            .contains("membership")
    );
    success(
        "REVOKE demo_owner FROM outsider;DROP ROLE outsider",
        "control",
        "postgres",
    );
    success("GRANT demo_owner TO demo_runtime", "control", "postgres");
    assert!(
        provision::apply(&policy, &endpoint)
            .unwrap_err()
            .to_string()
            .contains("membership")
    );
    success("REVOKE demo_owner FROM demo_runtime", "control", "postgres");
    let _lease = durable::lock(&lock, true, false).unwrap();
    assert!(provision::apply(&policy, &endpoint).is_err());
}
#[test]
fn inventory_rejects_redirects() {
    let t = tempfile::tempdir().unwrap();
    assert!(application_backup::inventory(t.path()).is_err());
    std::fs::write(t.path().join("data"), b"capture").unwrap();
    application_backup::inventory(t.path()).unwrap();
    std::os::unix::fs::symlink("data", t.path().join("link")).unwrap();
    assert!(application_backup::inventory(t.path()).is_err());
}
