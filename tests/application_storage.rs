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
    semantic=hashlib.sha256((backup/'records.json').read_bytes()).hexdigest()
    (backup/'capture.json').write_text(json.dumps({'version':1,'consistency':'quiesced','semantic_sha256':semantic}))
    if (base/'symlink').exists(): (backup/'redirect').symlink_to('/etc/passwd')
    if (base/'self_accept').exists(): (backup/'acceptance.json').write_text('{}')
    if (base/'incomplete_semantic').exists(): (backup/'capture.json').write_text('{}')
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

fn python_backup(c: &Value, action: &str) -> harbor_db::storage::Result<Value> {
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import application_backup
from unittest.mock import patch
if sys.argv[3]:
    original_digest=application_backup.digest
    patch.object(application_backup,'digest',side_effect=lambda path: original_digest(sys.argv[3] if str(path)=='/etc/machine-id' else path)).start()
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
            "--nocapture".into(),
        ]);
        let mut env: std::collections::BTreeMap<_, _> = std::env::vars().collect();
        env.insert(
            "HARBOR_DB_PROVISION_CHILD".into(),
            root.path().display().to_string(),
        );
        spec.environment = Some(env);
        spec.identity = identity;
        spec.timeout = Duration::from_secs(120);
        let output = process::run(&spec).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
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
    assert!(out.status.success());
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
    success(
        "CREATE TABLE documents(id int);CREATE TABLE history(id int);CREATE TABLE immutable(id int);CREATE SEQUENCE cursor;",
        "demo_owner",
        "demo",
    );
    provision::apply(&policy, &endpoint).unwrap();
    assert!(provision::check(&policy, &endpoint).unwrap());
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
    }
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
