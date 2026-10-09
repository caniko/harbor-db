use harbor_db::storage::writer_fence;
use harbor_db::storage::{codec, durable, startup_inhibition};
use serde_json::Value;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};
// Prevent unrelated fork/pre-exec windows retaining just-closed fixture leases.
// Explicit shared/exclusive contention remains exercised inside each test.
static WORKERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn executable(path: &Path, script: &str) {
    fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
struct Fixture {
    root: tempfile::TempDir,
    config: Value,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in ["data", "state", "package/bin"] {
            fs::create_dir_all(root.path().join(name)).unwrap();
        }
        fs::write(root.path().join("data/PG_VERSION"), "18\n").unwrap();
        fs::write(root.path().join("data/identifier"), "12345").unwrap();
        fs::write(
            root.path().join("data/postgresql.auto.conf"),
            b"# preserved\nwork_mem = '16MB'\n",
        )
        .unwrap();
        executable(
            &root.path().join("package/bin/pg_controldata"),
            "printf 'Database system identifier: '; cat \"$1/identifier\"; printf '\\n'",
        );
        executable(&root.path().join("package/bin/pg_ctl"), "exit 3");
        let config = json!({"resource":"fixture","major":"18","package":root.path().join("package"),"state_dir":root.path().join("state"),"data_dir":root.path().join("data"),"writer_fence":{"replication_roles":["replicator"]}});
        Self { root, config }
    }
    fn p(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn open(&self) -> Value {
        writer_fence::open_fence(&self.config, "12345").unwrap()
    }
    fn record(&self) -> Value {
        writer_fence::startup(&self.config).unwrap().unwrap()
    }
}

fn python_fence(config: &Value, action: &str, token: Option<&str>) -> Value {
    use harbor_db::storage::process;
    let script = r#"import json,sys
from pathlib import Path
from harbor_db import writer_fence
config,action,token=json.loads(sys.argv[1]),sys.argv[2],sys.argv[3]
if action == 'open': result=writer_fence.open_fence(config,'12345')
elif action == 'startup': result=writer_fence.startup(config)
elif action == 'close': result=writer_fence.close_fence(config,token)
elif action == 'closed': result=writer_fence.inspect_offline(config,token,'closed')
elif action == 'interrupt-close':
    real=writer_fence.atomic_write
    def interrupt(path,content):
        real(path,content)
        raise RuntimeError('fixture interruption after original selector restoration')
    writer_fence.atomic_write=interrupt
    try: writer_fence.close_fence(config,token)
    except RuntimeError as error:
        assert 'fixture interruption' in str(error)
    else: raise AssertionError('explicit interruption was not reached')
    result=json.loads(writer_fence.marker(config).read_bytes())
else: raise AssertionError(action)
print(json.dumps(result))
"#;
    let mut spec = process::CommandSpec::new(vec![
        "python3".into(),
        "-B".into(),
        "-c".into(),
        script.into(),
        config.to_string(),
        action.into(),
        token.unwrap_or("").into(),
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

#[test]
fn python_and_rust_resume_each_others_fence_receipts_and_exact_hba_bytes() {
    let _workers = WORKERS.lock().unwrap();
    for python_first in [true, false] {
        let f = Fixture::new();
        let original = fs::read(f.p("data/postgresql.auto.conf")).unwrap();
        let prepared = if python_first {
            python_fence(&f.config, "open", None)
        } else {
            f.open()
        };
        let token = prepared["token"].as_str().unwrap();
        let before = fs::read(writer_fence::marker(&f.config).unwrap()).unwrap();
        let native = f.record();
        assert_eq!(python_fence(&f.config, "startup", None), native);
        let hba = PathBuf::from(native["hba_file"].as_str().unwrap());
        assert_eq!(
            fs::read(hba).unwrap(),
            writer_fence::hba_contents(&native["policy"]).unwrap()
        );
        let repeated = if python_first {
            f.open()
        } else {
            python_fence(&f.config, "open", None)
        };
        assert_eq!(
            repeated, prepared,
            "resume must preserve the token and boundary receipt"
        );
        assert_eq!(
            fs::read(writer_fence::marker(&f.config).unwrap()).unwrap(),
            before
        );
        let closed = if python_first {
            writer_fence::close_fence(&f.config, token).unwrap()
        } else {
            python_fence(&f.config, "close", Some(token))
        };
        assert_eq!(closed["status"], "closed-offline");
        assert_eq!(
            fs::read(f.p("data/postgresql.auto.conf")).unwrap(),
            original
        );
        assert_eq!(
            python_fence(&f.config, "closed", Some(token)),
            writer_fence::inspect_offline(&f.config, token, "closed").unwrap()
        );
    }
}

#[test]
fn rust_finishes_python_interrupted_thaw_without_replacing_token_or_original_configuration() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let original = fs::read(f.p("data/postgresql.auto.conf")).unwrap();
    let prepared = f.open();
    let token = prepared["token"].as_str().unwrap();
    let interrupted = python_fence(&f.config, "interrupt-close", Some(token));
    assert_eq!(interrupted["phase"], "closing");
    assert_eq!(interrupted["token"], token);
    assert!(writer_fence::startup(&f.config).is_err());
    assert_eq!(
        fs::read(f.p("data/postgresql.auto.conf")).unwrap(),
        original
    );
    let closed = writer_fence::close_fence(&f.config, token).unwrap();
    assert_eq!(closed["token"], token);
    assert_eq!(
        python_fence(&f.config, "closed", Some(token)),
        writer_fence::inspect_offline(&f.config, token, "closed").unwrap()
    );
    assert_eq!(
        fs::read(f.p("data/postgresql.auto.conf")).unwrap(),
        original
    );
}

#[test]
fn policy_and_literal_reserved_roles_match_legacy_hba() {
    let policy = writer_fence::policy(&json!({"writer_fence":{"control_role":"all","replication_roles":["z","all"],"allowed_preload_libraries":["x","x"]}})).unwrap();
    assert_eq!(policy["replication_roles"], json!(["all", "z"]));
    assert_eq!(policy["allowed_preload_libraries"], json!(["x"]));
    let hba = String::from_utf8(writer_fence::hba_contents(&policy).unwrap()).unwrap();
    assert!(hba.contains("local all \"all\" peer\n"));
    assert!(hba.contains("host replication \"all\",\"z\" 127.0.0.1/32 scram-sha-256\n"));
    assert!(hba.ends_with("host replication all ::/0 reject\n"));
    for value in [
        json!({"replication_roles":["x","x"]}),
        json!({"control_role":"a\nb"}),
        json!({"allowed_preload_libraries":["../x"]}),
    ] {
        assert!(writer_fence::policy(&json!({"writer_fence":value})).is_err());
    }
}

#[test]
fn orphan_selector_and_hostile_auto_configuration_fail_closed() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let state = root.path().join("state");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&state).unwrap();
    let config = json!({"data_dir":data,"state_dir":state});
    let auto = data.join("postgresql.auto.conf");
    std::fs::write(
        &auto,
        b"# harbor-db-writer-fence=0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();
    assert!(writer_fence::startup(&config).is_err());
    std::fs::write(&auto, b"work_mem = '16MB'\n").unwrap();
    assert!(writer_fence::startup(&config).unwrap().is_none());
    std::fs::set_permissions(&auto, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(writer_fence::startup(&config).is_err());
    std::fs::remove_file(&auto).unwrap();
    symlink("missing", &auto).unwrap();
    assert!(writer_fence::startup(&config).is_err());
}

#[test]
fn offline_open_close_preserves_bytes_and_retained_history() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let original = fs::read(f.p("data/postgresql.auto.conf")).unwrap();
    let opened = f.open();
    assert_eq!(opened["status"], "prepared-offline");
    assert_eq!(opened["restart_required"], true);
    let token = opened["token"].as_str().unwrap();
    let record = f.record();
    assert_eq!(
        fs::read(record["original_file"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(
        fs::metadata(record["original_file"].as_str().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        writer_fence::open_fence(&f.config, "12345").unwrap()["token"],
        token
    );
    assert_eq!(
        writer_fence::inspect_offline(&f.config, token, "prepared").unwrap()["status"],
        "prepared-offline"
    );
    assert!(writer_fence::inspect_offline(&f.config, token, "closed").is_err());
    let closed = writer_fence::close_fence(&f.config, token).unwrap();
    assert_eq!(closed["restart_required"], true);
    assert_eq!(
        fs::read(f.p("data/postgresql.auto.conf")).unwrap(),
        original
    );
    assert!(writer_fence::startup(&f.config).unwrap().is_none());
    assert!(Path::new(record["hba_file"].as_str().unwrap()).exists());
    assert!(Path::new(closed["receipt"].as_str().unwrap()).exists());
    assert_eq!(
        writer_fence::inspect_offline(&f.config, token, "closed").unwrap()["system_identifier"],
        "12345"
    );
    fs::write(f.p("data/postgresql.auto.conf"), b"foreign").unwrap();
    assert!(writer_fence::inspect_offline(&f.config, token, "closed").is_err());
}

#[test]
fn stoppedness_identity_and_upgrade_are_required_before_publication() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let original = fs::read(f.p("data/postgresql.auto.conf")).unwrap();
    for status in [0, 1] {
        executable(&f.p("package/bin/pg_ctl"), &format!("exit {status}"));
        assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    }
    executable(&f.p("package/bin/pg_ctl"), "exit 3");
    fs::write(f.p("data/postmaster.pid"), "99999\n").unwrap();
    assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    fs::remove_file(f.p("data/postmaster.pid")).unwrap();
    assert!(writer_fence::open_fence(&f.config, "99999").is_err());
    assert!(writer_fence::open_fence(&f.config, "012345").is_err());
    fs::write(f.p("state/upgrade.json"), "{}").unwrap();
    assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    assert_eq!(
        fs::read(f.p("data/postgresql.auto.conf")).unwrap(),
        original
    );
    assert!(!f.p("state/writer-fence.json").exists());
    assert!(!f.p("state/identity.json").exists());
}

#[test]
fn interrupted_selector_commit_resumes_same_token_and_never_infers_readiness() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let record = f.record();
    fs::remove_file(f.p("state/writer-fence.json")).unwrap();
    assert!(
        writer_fence::startup(&f.config)
            .unwrap_err()
            .to_string()
            .contains("no journal")
    );
    assert_eq!(f.open()["token"], opened["token"]);
    fs::remove_file(f.p("state/writer-fence.json")).unwrap();
    fs::write(record["selected_file"].as_str().unwrap(), b"changed").unwrap();
    assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    assert!(!f.p("state/writer-fence.json").exists());
}

#[test]
fn prepared_artifacts_before_selector_commit_are_not_implicitly_selected() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let record = f.record();
    fs::remove_file(f.p("state/writer-fence.json")).unwrap();
    durable::atomic_write(
        &f.p("data/postgresql.auto.conf"),
        &fs::read(record["original_file"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(writer_fence::startup(&f.config).unwrap().is_none());
    let new = f.open();
    assert_ne!(new["token"], opened["token"]);
    assert!(Path::new(record["hba_file"].as_str().unwrap()).exists());
}

#[test]
fn interrupted_thaw_blocks_start_and_resumes_after_original_selector_commit() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let token = opened["token"].as_str().unwrap();
    let mut record = f.record();
    record["phase"] = json!("closing");
    durable::write_json(&f.p("state/writer-fence.json"), &record).unwrap();
    assert!(writer_fence::startup(&f.config).is_err());
    assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    durable::atomic_write(
        &f.p("data/postgresql.auto.conf"),
        &fs::read(record["original_file"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(writer_fence::inspect_offline(&f.config, token, "closed").is_err());
    assert_eq!(
        writer_fence::close_fence(&f.config, token).unwrap()["status"],
        "closed-offline"
    );
    assert!(writer_fence::startup(&f.config).unwrap().is_none());
}

#[test]
fn thaw_rejects_wrong_token_running_primary_foreign_selector_and_hostile_receipt() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let token = opened["token"].as_str().unwrap();
    assert!(writer_fence::close_fence(&f.config, &"0".repeat(32)).is_err());
    executable(&f.p("package/bin/pg_ctl"), "exit 0");
    assert!(writer_fence::close_fence(&f.config, token).is_err());
    executable(&f.p("package/bin/pg_ctl"), "exit 3");
    let selected = fs::read(f.p("data/postgresql.auto.conf")).unwrap();
    let mut foreign = selected.clone();
    foreign.extend(b"work_mem = '32MB'\n");
    fs::write(f.p("data/postgresql.auto.conf"), &foreign).unwrap();
    assert!(writer_fence::close_fence(&f.config, token).is_err());
    assert_eq!(fs::read(f.p("data/postgresql.auto.conf")).unwrap(), foreign);
    fs::write(f.p("data/postgresql.auto.conf"), selected).unwrap();
    let closed = writer_fence::close_fence(&f.config, token).unwrap();
    let receipt = Path::new(closed["receipt"].as_str().unwrap());
    let mut bad = durable::read_json(receipt).unwrap();
    bad["unexpected"] = json!(true);
    durable::write_json(receipt, &bad).unwrap();
    assert!(writer_fence::inspect_offline(&f.config, token, "closed").is_err());
}

#[test]
fn changed_private_artifacts_binding_permissions_and_symlinks_fail_closed() {
    let _workers = WORKERS.lock().unwrap();
    for key in ["original_file", "selected_file", "hba_file"] {
        let f = Fixture::new();
        f.open();
        let record = f.record();
        let file = Path::new(record[key].as_str().unwrap());
        fs::write(file, b"changed").unwrap();
        assert!(writer_fence::startup(&f.config).is_err(), "{key}");
    }
    let f = Fixture::new();
    f.open();
    fs::write(f.p("data/identifier"), "67890").unwrap();
    assert!(writer_fence::startup(&f.config).is_err());
    fs::write(f.p("data/identifier"), "12345").unwrap();
    let mut config = f.config.clone();
    config["writer_fence"]["replication_roles"] = json!([]);
    assert!(writer_fence::startup(&config).is_err());
    let record = f.record();
    let hba = Path::new(record["hba_file"].as_str().unwrap());
    fs::set_permissions(hba, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(writer_fence::startup(&f.config).is_err());
    fs::set_permissions(hba, fs::Permissions::from_mode(0o600)).unwrap();
    let saved = hba.with_extension("saved");
    fs::rename(hba, &saved).unwrap();
    symlink(&saved, hba).unwrap();
    assert!(writer_fence::startup(&f.config).is_err());
}

#[test]
fn malformed_tokens_and_forged_hba_contract_cannot_change_journal_paths() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    f.open();
    let mut record = f.record();
    let hba = Path::new(record["hba_file"].as_str().unwrap());
    fs::write(hba, b"local all all trust\n").unwrap();
    record["hba_sha256"] = json!(codec::file_digest(hba).unwrap());
    durable::write_json(&f.p("state/writer-fence.json"), &record).unwrap();
    assert!(writer_fence::startup(&f.config).is_err());
    for token in ["../escape", "a\n", "fffffffffffffffffffffffffffffffff"] {
        record["token"] = json!(token);
        durable::write_json(&f.p("state/writer-fence.json"), &record).unwrap();
        assert!(writer_fence::startup(&f.config).is_err());
    }
}

#[test]
fn shared_fence_lease_prevents_prepare_and_thaw_but_allows_inspection() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let lease = durable::lock(&f.p("state/writer-fence.lock"), true, false).unwrap();
    assert!(writer_fence::open_fence(&f.config, "12345").is_err());
    assert!(writer_fence::close_fence(&f.config, opened["token"].as_str().unwrap()).is_err());
    writer_fence::inspect_offline(&f.config, opened["token"].as_str().unwrap(), "prepared")
        .unwrap();
    drop(lease);
    writer_fence::close_fence(&f.config, opened["token"].as_str().unwrap()).unwrap();
}

#[test]
fn live_fence_rejects_every_nonexclusive_or_mismatched_observation() {
    let _workers = WORKERS.lock().unwrap();
    let f = Fixture::new();
    let opened = f.open();
    let record = f.record();
    let token = opened["token"].as_str().unwrap();
    let observation = json!({"data_dir":f.config["data_dir"],"major":"18","system_identifier":"12345","hba_file":record["hba_file"],"control_role":"postgres","in_recovery":false,"fsync":"on","full_page_writes":"on","synchronous_commit":"on","preload_libraries":[],"logical_subscriptions":0,"prepared_transactions":0,"other_writers":0});
    executable(
        &f.p("package/bin/psql"),
        &format!("cat '{}'", f.p("live.json").display()),
    );
    fs::write(f.p("live.json"), observation.to_string()).unwrap();
    assert_eq!(
        writer_fence::inspect_live(&f.config, token, Path::new("/run/postgresql"), 5432).unwrap()["status"],
        "ready"
    );
    for (key, bad) in [
        ("data_dir", json!("/other")),
        ("major", json!("17")),
        ("system_identifier", json!("99999")),
        ("hba_file", json!("/foreign")),
        ("control_role", json!("application")),
        ("in_recovery", json!(true)),
        ("fsync", json!("off")),
        ("full_page_writes", json!("off")),
        ("synchronous_commit", json!("off")),
        ("preload_libraries", json!(["unsafe"])),
        ("logical_subscriptions", json!(1)),
        ("prepared_transactions", json!(1)),
        ("other_writers", json!(1)),
    ] {
        let mut bad_observation = observation.clone();
        bad_observation[key] = bad;
        fs::write(f.p("live.json"), bad_observation.to_string()).unwrap();
        assert!(
            writer_fence::inspect_live(&f.config, token, Path::new("/run/postgresql"), 5432)
                .is_err(),
            "{key}"
        );
    }
    assert!(writer_fence::inspect_live(&f.config, token, Path::new("localhost"), 5432).is_err());
}

#[test]
fn inhibition_requires_effective_nontrigger_negated_condition_and_root() {
    let state = Path::new("/trusted/root-startup");
    let good = json!({"type":"a(sbbsi)","data":[["ConditionPathExists",false,true,state.join("inhibited.json"),0]]});
    startup_inhibition::effective_condition(&good, state).unwrap();
    for bad in [
        json!({"type":"wrong","data":good["data"]}),
        json!({"type":"a(sbbsi)","data":[]}),
        json!({"type":"a(sbbsi)","data":[["ConditionPathExists",true,true,state.join("inhibited.json"),0]]}),
        json!({"type":"a(sbbsi)","data":[["ConditionPathExists",false,false,state.join("inhibited.json"),0]]}),
        json!({"type":"a(sbbsi)","data":[["ConditionPathExists",false,true,"/foreign",0]]}),
    ] {
        assert!(startup_inhibition::effective_condition(&bad, state).is_err());
    }
    assert_eq!(startup_inhibition::content(state),b"# Harbor DB: retained across legacy and guarded generations.\n[Unit]\nConditionPathExists=!/trusted/root-startup/inhibited.json\n");
    if unsafe { libc::geteuid() } != 0 {
        assert!(
            startup_inhibition::inhibit(&json!({}), "12345")
                .unwrap_err()
                .to_string()
                .contains("requires root")
        );
        assert!(
            startup_inhibition::release(
                &json!({}),
                Path::new("/manifest"),
                "token",
                "fence",
                "prepared"
            )
            .unwrap_err()
            .to_string()
            .contains("requires root")
        );
    }
}

#[test]
fn native_fence_survives_bare_postmaster_restart_and_requires_stopped_thaw() {
    let _workers = WORKERS.lock().unwrap();
    use harbor_db::storage::{
        pg_core,
        process::{self, CommandSpec, Identity},
    };
    use std::time::Duration;
    let package = PathBuf::from(
        std::env::var("HARBOR_DB_TEST_POSTGRES")
            .expect("set HARBOR_DB_TEST_POSTGRES for real PostgreSQL qualification"),
    );
    if std::env::var_os("HARBOR_DB_NATIVE_FENCE_CHILD").is_none() {
        let root = tempfile::tempdir_in(
            std::env::var_os("HARBOR_DB_TEST_TMPDIR")
                .unwrap_or_else(|| "/data/scratch/tmp/opencode".into()),
        )
        .unwrap();
        let identity = if unsafe { libc::geteuid() } == 0 {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            let path = CString::new(root.path().as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);
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
            "native_fence_survives_bare_postmaster_restart_and_requires_stopped_thaw".into(),
            "--nocapture".into(),
        ]);
        let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        env.insert(
            "HARBOR_DB_NATIVE_FENCE_CHILD".into(),
            root.path().display().to_string(),
        );
        spec.environment = Some(env);
        spec.identity = identity;
        spec.timeout = Duration::from_secs(90);
        let output = process::run(&spec).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        println!("native fence qualification failure: {info}")
    }));
    let root = PathBuf::from(std::env::var("HARBOR_DB_NATIVE_FENCE_CHILD").unwrap());
    let data = root.join("data");
    let state = root.join("state");
    let socket = root.join("socket");
    for directory in [&state, &socket] {
        fs::create_dir(directory).unwrap();
    }
    let role = process::current_username().unwrap();
    let run = |program: &str, args: Vec<String>| {
        let mut spec = pg_core::command(
            std::iter::once(package.join("bin").join(program).display().to_string())
                .chain(args)
                .collect(),
            true,
        );
        spec.timeout = Duration::from_secs(30);
        process::run(&spec).unwrap()
    };
    assert!(
        run(
            "initdb",
            vec![
                "-D".into(),
                data.display().to_string(),
                "-U".into(),
                role.clone(),
                "--locale=C".into(),
                "--auth=trust".into()
            ]
        )
        .status
        .success()
    );
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(data.join("postgresql.conf"))
            .unwrap(),
        "\nunix_socket_directories = '{}'\nport = 55443\nlisten_addresses = ''",
        socket.display()
    )
    .unwrap();
    let config = json!({"resource":"native-fence","major":"18","package":package,"data_dir":data,"state_dir":state,"writer_fence":{"control_role":role}});
    let identifier = pg_core::inspect_cluster(&package, &data, "18").unwrap();
    let original = fs::read(data.join("postgresql.auto.conf")).unwrap();
    let opened = writer_fence::open_fence(&config, &identifier).unwrap();
    let token = opened["token"].as_str().unwrap();
    struct Stop<'a> {
        package: &'a Path,
        data: &'a Path,
    }
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let mut spec = pg_core::command(
                vec![
                    self.package.join("bin/pg_ctl").display().to_string(),
                    "-D".into(),
                    self.data.display().to_string(),
                    "-m".into(),
                    "fast".into(),
                    "-w".into(),
                    "stop".into(),
                ],
                true,
            );
            spec.timeout = Duration::from_secs(30);
            let _ = process::run(&spec);
        }
    }
    for _ in 0..2 {
        // Bare pg_ctl models a legacy generation: auto.conf must retain the HBA.
        assert!(
            run(
                "pg_ctl",
                vec![
                    "-D".into(),
                    data.display().to_string(),
                    "-l".into(),
                    "/dev/null".into(),
                    "-w".into(),
                    "start".into()
                ]
            )
            .status
            .success()
        );
        let stop = Stop {
            package: &package,
            data: &data,
        };
        assert_eq!(
            writer_fence::inspect_live(&config, token, &socket, 55443).unwrap()["status"],
            "ready"
        );
        assert!(writer_fence::close_fence(&config, token).is_err());
        assert!(writer_fence::open_fence(&config, &identifier).is_err());
        let endpoint = vec![
            "-X".into(),
            "-At".into(),
            "-h".into(),
            socket.display().to_string(),
            "-p".into(),
            "55443".into(),
            "-d".into(),
            "postgres".into(),
        ];
        let mut control = endpoint.clone();
        control.extend(["-U".into(), role.clone(), "-c".into(), "SELECT 1".into()]);
        assert!(run("psql", control).status.success());
        let mut application = endpoint;
        application.extend([
            "-U".into(),
            "application".into(),
            "-c".into(),
            "SELECT 1".into(),
        ]);
        assert!(!run("psql", application).status.success());
        drop(stop);
        assert_eq!(
            writer_fence::open_fence(&config, &identifier).unwrap()["token"],
            token
        );
    }
    writer_fence::close_fence(&config, token).unwrap();
    assert_eq!(
        fs::read(data.join("postgresql.auto.conf")).unwrap(),
        original
    );
    assert_eq!(
        writer_fence::inspect_offline(&config, token, "closed").unwrap()["status"],
        "closed-offline"
    );
}
