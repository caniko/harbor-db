use harbor_db::storage::{durable, pg_core, postgres};
use serde_json::json;
use std::{fs, os::unix::fs::symlink};

// A fork briefly inherits every open description before exec applies CLOEXEC.
// Serialize subprocess fixtures so an unrelated concurrent fork cannot retain
// another test's just-released flock during its immediate idempotent retry.
static WORKERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn executable(path: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn synthetic() -> (
    tempfile::TempDir,
    serde_json::Value,
    std::sync::MutexGuard<'static, ()>,
) {
    let guard = WORKERS.lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    for name in [
        "state",
        "data",
        "package/bin",
        "data/global",
        "data/pg_tblspc",
        "data/pg_wal",
    ] {
        fs::create_dir_all(root.path().join(name)).unwrap();
    }
    fs::write(root.path().join("data/PG_VERSION"), "18\n").unwrap();
    fs::write(
        root.path().join("data/global/pg_control"),
        "source unchanged",
    )
    .unwrap();
    fs::write(root.path().join("data/identifier"), "12345").unwrap();
    executable(
        &root.path().join("package/bin/pg_controldata"),
        "printf 'Database system identifier: '; cat \"$1/identifier\"; printf '\\n'",
    );
    executable(&root.path().join("package/bin/pg_ctl"), "exit 3");
    executable(&root.path().join("package/bin/initdb"), "exit 1");
    executable(&root.path().join("package/bin/pg_upgrade"), "exit 1");
    executable(&root.path().join("package/bin/postgres"), "exit 1");
    let config = json!({"resource":"db","major":"18","package":root.path().join("package"),"state_dir":root.path().join("state"),"data_dir":root.path().join("data")});
    (root, config, guard)
}

#[test]
fn operator_upgrade_rejects_mutable_tools_and_adapters_before_state_changes_in_both_engines() {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    let root = tempfile::tempdir().unwrap();
    let package = PathBuf::from(std::env::var("HARBOR_DB_TEST_POSTGRES").unwrap());
    let old_package = PathBuf::from(std::env::var("HARBOR_DB_TEST_POSTGRES_17").unwrap());
    let validator = package.join("bin/pg_isready");
    let base = json!({"resource":"upgrade","major":"18","package":package,"state_dir":root.path().join("authority"),"data_dir":root.path().join("new"),"upgrade":{"data_dir":root.path().join("old"),"major":"17","package":old_package,"validate_command":[validator]}});
    postgres::validate_operator_upgrade_policy(&base).unwrap();
    let writable = root.path().join("validator");
    executable(&writable, "exit 0");
    let alias = root.path().join("alias");
    symlink(&validator, &alias).unwrap();
    let cases = [
        ("mutable-validator", json!([writable]), None),
        ("mutable-alias", json!([alias]), None),
        (
            "mutable-interpreter-argument",
            json!([validator, writable]),
            None,
        ),
        (
            "declared-mutable-adapter",
            json!([validator]),
            Some(json!([writable])),
        ),
        (
            "nonexecutable-validator",
            json!([package.join("share/postgresql/postgres.bki")]),
            None,
        ),
    ];
    for (name, command, files) in cases {
        let mut config = base.clone();
        config["upgrade"]["validate_command"] = command;
        if let Some(files) = files {
            config["upgrade"]["executable_files"] = files;
        }
        let manifest = root.path().join("config.json");
        fs::write(&manifest, config.to_string()).unwrap();
        let native = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"))
            .arg("--config")
            .arg(&manifest)
            .arg("upgrade")
            .output()
            .unwrap();
        assert!(!native.status.success(), "{name} accepted by native CLI");
        assert!(
            String::from_utf8_lossy(&native.stderr).contains("immutable store"),
            "{name}: {}",
            String::from_utf8_lossy(&native.stderr)
        );
        let script = r#"import json,os,sys
from pathlib import Path
from unittest.mock import patch
from harbor_db.postgres import main
root_uid=int(sys.argv[2])
if root_uid:
    original_lstat=Path.lstat
    def mapped_lstat(path):
        info=original_lstat(path)
        if info.st_uid!=root_uid: return info
        fields=list(info); fields[4]=0
        return os.stat_result(fields)
    patch.object(Path,'lstat',mapped_lstat).start()
sys.argv=['harbor-db-postgres','--config',sys.argv[1],'upgrade']
raise SystemExit(main())
"#;
        let python = Command::new("python3")
            .args(["-B", "-c", script])
            .arg(&manifest)
            .arg(option_env!("HARBOR_DB_TEST_ROOT_UID").unwrap_or("0"))
            .env(
                "PYTHONPATH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python"),
            )
            .output()
            .unwrap();
        assert!(!python.status.success(), "{name} accepted by Python CLI");
        assert!(
            String::from_utf8_lossy(&python.stderr).contains("immutable store"),
            "{name}: {}",
            String::from_utf8_lossy(&python.stderr)
        );
        assert!(
            !root.path().join("authority").exists(),
            "{name} mutated authority before rejection"
        );
        assert!(
            !root.path().join("new").exists() && !root.path().join("new.harbor-staging").exists()
        );
    }
}

#[test]
fn adoption_rejects_replacement_and_missing_lock_anchor() {
    let (root, config, _guard) = synthetic();
    assert!(postgres::check(&config).is_err());
    assert!(postgres::adopt(&config, "99999").is_err());
    postgres::adopt(&config, "12345").unwrap();
    postgres::adopt(&config, "12345").unwrap();
    postgres::check(&config).unwrap();
    fs::write(root.path().join("data/identifier"), "67890").unwrap();
    assert!(postgres::check(&config).is_err());
    assert!(postgres::adopt(&config, "67890").is_err());
    fs::write(root.path().join("data/identifier"), "12345").unwrap();
    let lease = durable::lock(&root.path().join("state/lock"), true, false).unwrap();
    fs::remove_file(root.path().join("state/lock")).unwrap();
    assert!(postgres::adopt(&config, "12345").is_err());
    assert!(!root.path().join("state/lock").exists());
    drop(lease);
}

#[test]
fn live_identity_is_read_only_and_rejects_each_nondurable_setting() {
    let (root, config, _guard) = synthetic();
    let observation = json!({"data_dir":config["data_dir"],"major":"18","system_identifier":"12345","fsync":"on","full_page_writes":"on","synchronous_commit":"on","in_recovery":false});
    let receipt = root.path().join("live.json");
    fs::write(&receipt, observation.to_string()).unwrap();
    executable(
        &root.path().join("package/bin/psql"),
        &format!("cat '{}'", receipt.display()),
    );
    pg_core::inspect_live(
        &config,
        "12345",
        std::path::Path::new("/run/postgresql"),
        5432,
    )
    .unwrap();
    assert!(!root.path().join("state/identity.json").exists());
    assert!(!root.path().join("state/lock").exists());
    for (key, bad) in [
        ("data_dir", json!("/wrong")),
        ("major", json!("17")),
        ("system_identifier", json!("67890")),
        ("fsync", json!("off")),
        ("full_page_writes", json!("off")),
        ("synchronous_commit", json!("off")),
        ("in_recovery", json!(true)),
    ] {
        let mut changed = observation.clone();
        changed[key] = bad;
        fs::write(&receipt, changed.to_string()).unwrap();
        assert!(
            postgres::adopt_live(
                &config,
                "12345",
                std::path::Path::new("/run/postgresql"),
                5432
            )
            .is_err(),
            "{key}"
        );
        assert!(!root.path().join("state/identity.json").exists());
    }
    fs::write(&receipt, observation.to_string()).unwrap();
    assert_eq!(
        postgres::adopt_live(
            &config,
            "12345",
            std::path::Path::new("/run/postgresql"),
            5432
        )
        .unwrap()["changed"],
        true
    );
    let original = fs::read(root.path().join("state/identity.json")).unwrap();
    let _lease = durable::lock(&root.path().join("state/lock"), true, false).unwrap();
    assert_eq!(
        postgres::adopt_live(
            &config,
            "12345",
            std::path::Path::new("/run/postgresql"),
            5432
        )
        .unwrap()["changed"],
        false
    );
    assert_eq!(
        fs::read(root.path().join("state/identity.json")).unwrap(),
        original
    );
}

#[test]
fn inspection_workers_inherit_every_supplied_lease() {
    let (root, config, _guard) = synthetic();
    let first = durable::lock(&root.path().join("first.lock"), false, true).unwrap();
    let second = durable::lock(&root.path().join("second.lock"), false, true).unwrap();
    let leases = [first.fd(), second.fd()];
    let checks = format!(
        "test -e /proc/self/fd/{} || exit 1\ntest -e /proc/self/fd/{} || exit 1\n",
        first.fd(),
        second.fd()
    );
    executable(
        &root.path().join("package/bin/pg_controldata"),
        &format!("{checks}printf 'Database system identifier: 12345\\n'"),
    );
    assert!(
        pg_core::inspect_cluster(
            &root.path().join("package"),
            &root.path().join("data"),
            "18"
        )
        .is_err()
    );
    pg_core::inspect_cluster_leased(
        &root.path().join("package"),
        &root.path().join("data"),
        "18",
        &leases,
    )
    .unwrap();
    let observed = json!({"data_dir":config["data_dir"],"major":"18","system_identifier":"12345","fsync":"on","full_page_writes":"on","synchronous_commit":"on","in_recovery":false});
    executable(
        &root.path().join("package/bin/psql"),
        &format!("{checks}printf '%s\\n' '{}'", observed),
    );
    pg_core::inspect_live_leased(
        &config,
        "12345",
        std::path::Path::new("/run/postgresql"),
        5432,
        &leases,
    )
    .unwrap();
    executable(
        &root.path().join("package/bin/pg_ctl"),
        &format!("{checks}exit 3"),
    );
    assert!(
        pg_core::require_stopped(&root.path().join("package"), &root.path().join("data")).is_err()
    );
    pg_core::require_stopped_leased(
        &root.path().join("package"),
        &root.path().join("data"),
        &leases,
    )
    .unwrap();
}

#[test]
fn interrupted_upgrade_is_preserved_and_requires_explicit_retry() {
    let (root, mut config, _guard) = synthetic();
    let source = root.path().join("data");
    fs::write(source.join("PG_VERSION"), "17\n").unwrap();
    let mut old = config.clone();
    old["major"] = json!("17");
    postgres::adopt(&old, "12345").unwrap();
    config["data_dir"] = json!(root.path().join("target"));
    let validator = root.path().join("validator");
    executable(&validator, "exit 0");
    config["upgrade"] = json!({"data_dir":source,"major":"17","package":config["package"],"validate_command":[validator]});
    executable(
        &root.path().join("package/bin/initdb"),
        "mkdir -p \"$2\"; printf '18\\n' > \"$2/PG_VERSION\"; printf 'staged output' > \"$2/evidence\"; exit 1",
    );
    assert!(postgres::upgrade(&config, false).is_err());
    let stage = root.path().join("target.harbor-staging");
    assert!(stage.join("evidence").exists());
    assert_eq!(
        fs::read(source.join("global/pg_control")).unwrap(),
        b"source unchanged"
    );
    assert_eq!(
        durable::read_json(&root.path().join("state/upgrade.json")).unwrap()["phase"],
        "building"
    );
    assert!(
        postgres::upgrade(&config, false)
            .unwrap_err()
            .to_string()
            .contains("retry-incomplete")
    );
    assert!(postgres::check(&old).is_err());
    assert!(postgres::upgrade(&config, true).is_err());
    assert!(
        root.path()
            .join("target.harbor-staging.interrupted/evidence")
            .exists()
    );
    assert!(
        root.path()
            .join("target.harbor-source.interrupted/global/pg_control")
            .exists()
    );
    assert!(
        postgres::upgrade(&config, true)
            .unwrap_err()
            .to_string()
            .contains("preserved")
    );
}

#[test]
fn nested_upgrade_paths_reject_before_lock_or_copy_in_both_engines() {
    for (source_name, target_name) in [
        ("data", "data/new"),
        ("data/old", "data"),
        ("data", "data"),
        ("data", "data/new/target"),
        ("data.harbor-staging/old", "data"),
        ("data.harbor-source/old", "data"),
    ] {
        let (root, mut config, _guard) = synthetic();
        let source = root.path().join(source_name);
        let target = root.path().join(target_name);
        fs::create_dir_all(&source).unwrap();
        config["data_dir"] = json!(target);
        config["upgrade"] = json!({"data_dir":source,"major":"17","package":root.path().join("package"),"validate_command":["/unused-validator"]});
        // No authority anchor exists. Reject before acquiring one, so the
        // default recursive copier cannot consume space on the predecessor.
        let native = postgres::upgrade(&config, false).unwrap_err();
        assert!(native.to_string().contains("disjoint"), "{native}");
        let script = r#"import json,sys
from harbor_db import postgres
try: postgres.upgrade(json.loads(sys.argv[1]))
except postgres.LifecycleError as error: assert 'disjoint' in str(error), str(error)
else: raise AssertionError('nested upgrade accepted')
"#;
        let mut command = harbor_db::storage::process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            script.into(),
            config.to_string(),
        ]);
        command.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        harbor_db::storage::process::execute(&command).unwrap();
        assert!(!root.path().join("state/lock").exists());
        assert!(!root.path().join("state/upgrade.json").exists());
    }
}

fn interrupted_ready_upgrade() -> (
    tempfile::TempDir,
    serde_json::Value,
    std::sync::MutexGuard<'static, ()>,
) {
    let (root, mut config, guard) = synthetic();
    let old = root.path().join("data");
    fs::write(old.join("PG_VERSION"), "17\n").unwrap();
    config["data_dir"] = json!(root.path().join("18"));
    let validator = root.path().join("validate");
    executable(
        &validator,
        &format!("printf checked >> '{}/validated'", root.path().display()),
    );
    config["upgrade"] = json!({"data_dir":old,"major":"17","package":root.path().join("package"),"validate_command":[validator],"initdb_args":[],"extra_config":""});
    let mut source = config.clone();
    source["data_dir"] = json!(old);
    source["major"] = json!("17");
    postgres::adopt(&source, "12345").unwrap();
    executable(
        &root.path().join("package/bin/initdb"),
        "mkdir -p \"$2/global\"; printf '18\\n' > \"$2/PG_VERSION\"; printf 12345 > \"$2/identifier\"; printf control > \"$2/global/pg_control\"; touch \"$2/postgresql.conf\"",
    );
    executable(&root.path().join("package/bin/pg_upgrade"), "exit 0");
    executable(
        &root.path().join("package/bin/pg_ctl"),
        &format!(
            "if test -e '{0}/block-publication' && grep -q '\"phase\": \"ready\"' '{0}/state/upgrade.json'; then exit 0; fi; exit 3",
            root.path().display()
        ),
    );
    fs::write(root.path().join("block-publication"), "").unwrap();
    assert!(
        postgres::upgrade(&config, false)
            .unwrap_err()
            .to_string()
            .contains("running")
    );
    assert_eq!(
        durable::read_json(&root.path().join("state/upgrade.json")).unwrap()["phase"],
        "ready"
    );
    assert!(!root.path().join("18").exists());
    (root, config, guard)
}

#[test]
fn ready_upgrade_binds_policy_and_executable_bytes_across_native_and_python_resume() {
    let (root, config, _guard) = interrupted_ready_upgrade();
    fs::remove_file(root.path().join("block-publication")).unwrap();
    let journal = fs::read(root.path().join("state/upgrade.json")).unwrap();
    let identity = fs::read(root.path().join("state/identity.json")).unwrap();
    let script = r#"import json,sys
from harbor_db import postgres
try: postgres.upgrade(json.loads(sys.argv[1]))
except postgres.LifecycleError as error: assert 'contract changed' in str(error), str(error)
else: raise AssertionError('changed ready upgrade was published')
"#;
    let check = |changed: &serde_json::Value| {
        let before = fs::read(root.path().join("state/upgrade.json")).unwrap();
        let native = postgres::upgrade(changed, false).unwrap_err();
        assert!(native.to_string().contains("contract changed"), "{native}");
        let mut command = harbor_db::storage::process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            script.into(),
            changed.to_string(),
        ]);
        command.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        harbor_db::storage::process::execute(&command).unwrap();
        assert_eq!(
            fs::read(root.path().join("state/upgrade.json")).unwrap(),
            before
        );
        assert_eq!(
            fs::read(root.path().join("state/identity.json")).unwrap(),
            identity
        );
        assert!(!root.path().join("18").exists());
        assert!(root.path().join("18.harbor-staging").exists());
    };
    for (key, value) in [
        ("copy_command", json!([root.path().join("validate")])),
        ("initdb_args", json!(["--no-data-checksums"])),
        ("extra_config", json!("shared_buffers = '64MB'")),
        (
            "validate_command",
            json!([root.path().join("validate"), "changed-arg"]),
        ),
    ] {
        let mut changed = config.clone();
        changed["upgrade"][key] = value;
        check(&changed);
    }
    for path in [
        root.path().join("validate"),
        root.path().join("package/bin/pg_upgrade"),
        root.path().join("package/bin/initdb"),
        root.path().join("package/bin/pg_ctl"),
        root.path().join("package/bin/pg_controldata"),
        root.path().join("package/bin/postgres"),
    ] {
        let before = fs::read(&path).unwrap();
        let mut after = before.clone();
        after.extend_from_slice(b"\n# changed validator or tool\n");
        fs::write(&path, after).unwrap();
        check(&config);
        fs::write(&path, before).unwrap();
    }
    let mut legacy: serde_json::Value = serde_json::from_slice(&journal).unwrap();
    legacy["version"] = json!(1);
    legacy.as_object_mut().unwrap().remove("contract");
    durable::write_json(&root.path().join("state/upgrade.json"), &legacy).unwrap();
    check(&config);
    fs::write(root.path().join("state/upgrade.json"), &journal).unwrap();
    // The exact contract can be resumed by the other engine after rejection.
    let mut command = harbor_db::storage::process::CommandSpec::new(vec!["python3".into(),"-B".into(),"-c".into(),"import json,sys; from harbor_db import postgres; postgres.upgrade(json.loads(sys.argv[1]))".into(),config.to_string()]);
    command.environment = Some(std::collections::BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]));
    harbor_db::storage::process::execute(&command).unwrap();
    assert!(!root.path().join("state/upgrade.json").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("validated")).unwrap(),
        "checked"
    );
    postgres::upgrade(&config, false).unwrap();
}

#[test]
fn ready_upgrade_resumes_after_rename_and_identity_publication() {
    let (root, mut config, _guard) = synthetic();
    let source = root.path().join("data");
    fs::write(source.join("PG_VERSION"), "17\n").unwrap();
    let mut old = config.clone();
    old["major"] = json!("17");
    postgres::adopt(&old, "12345").unwrap();
    config["data_dir"] = json!(root.path().join("target"));
    let validator = root.path().join("validator");
    executable(&validator, "exit 0");
    config["upgrade"] = json!({"data_dir":source,"major":"17","package":config["package"],"validate_command":[validator]});
    let stage = root.path().join("target.harbor-staging");
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("PG_VERSION"), "18\n").unwrap();
    fs::write(stage.join("identifier"), "54321").unwrap();
    let journal = json!({"version":2,"phase":"ready","source":pg_core::identity(&old,"12345").unwrap(),"source_control":pg_core::control_digest(&source).unwrap(),"target":{"resource":config["resource"],"major":config["major"],"package":config["package"],"data_dir":config["data_dir"]},"staging":stage,"source_copy":root.path().join("target.harbor-source"),"identity":pg_core::identity(&config,"54321").unwrap(),"contract":postgres::upgrade_contract(&config).unwrap()});
    durable::write_json(&root.path().join("state/upgrade.json"), &journal).unwrap();
    fs::rename(&stage, root.path().join("target")).unwrap();
    durable::write_json(
        &root.path().join("state/identity.json"),
        &journal["identity"],
    )
    .unwrap();
    postgres::upgrade(&config, false).unwrap();
    postgres::check(&config).unwrap();
    assert!(!root.path().join("state/upgrade.json").exists());
    assert_eq!(
        durable::read_json(&root.path().join("state/previous-identity.json")).unwrap(),
        journal["source"]
    );
    assert!(source.exists());
}

#[test]
fn native_postmaster_retains_main_pid_lease_and_logical_restore_is_private() {
    if let Some(root) = std::env::var_os("HARBOR_DB_CUSTODY_SQL_CHILD") {
        std::panic::set_hook(Box::new(|info| {
            println!("custody SQL qualification failure: {info}")
        }));
        custody_sql_boundary(&std::path::PathBuf::from(root));
        return;
    }
    let _workers = WORKERS.lock().unwrap();
    use harbor_db::storage::{
        postgres_drill,
        process::{self, CommandSpec, Identity},
    };
    use std::{
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let package = PathBuf::from(
        std::env::var("HARBOR_DB_TEST_POSTGRES")
            .expect("set HARBOR_DB_TEST_POSTGRES for real PostgreSQL qualification"),
    );
    if std::env::var_os("HARBOR_DB_NATIVE_CHILD").is_none() {
        let root = tempfile::tempdir_in(
            std::env::var_os("HARBOR_DB_TEST_TMPDIR")
                .unwrap_or_else(|| "/data/scratch/tmp/opencode".into()),
        )
        .unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        // PostgreSQL refuses root. Run the entire assertion body as an isolated uid.
        let identity = if unsafe { libc::getuid() } == 0 {
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
            "native_postmaster_retains_main_pid_lease_and_logical_restore_is_private".into(),
            "--nocapture".into(),
        ]);
        let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        env.insert(
            "HARBOR_DB_NATIVE_CHILD".into(),
            root.path().display().to_string(),
        );
        env.insert("PGUSER".into(), "postgres".into());
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
    std::panic::set_hook(Box::new(|info| {
        println!("native qualification failure: {info}")
    }));
    let root = PathBuf::from(std::env::var("HARBOR_DB_NATIVE_CHILD").unwrap());
    let run = |program: &str, args: Vec<String>| {
        let mut spec = pg_core::command(
            std::iter::once(package.join("bin").join(program).display().to_string())
                .chain(args)
                .collect(),
            true,
        );
        spec.timeout = Duration::from_secs(30);
        let output = process::run(&spec).unwrap();
        assert!(output.status.success(), "{program} failed");
        process::text(&output.stdout).unwrap()
    };
    let data = root.join("data");
    let state = root.join("state");
    let socket = root.join("socket");
    let backup = root.join("backup");
    let workspace = root.join("workspace");
    for path in [&state, &socket, &backup, &workspace] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    run(
        "initdb",
        vec![
            "-D".into(),
            data.display().to_string(),
            "-U".into(),
            "postgres".into(),
            "--locale=C".into(),
            "--encoding=UTF8".into(),
            "--auth=trust".into(),
        ],
    );
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(data.join("postgresql.conf"))
            .unwrap(),
        "\nunix_socket_directories = '{}'\nport = 55440\nlisten_addresses = ''",
        socket.display()
    )
    .unwrap();
    let config = json!({"resource":"native","major":"18","package":package,"state_dir":state,"data_dir":data});
    let id = pg_core::inspect_cluster(&package, &data, "18").unwrap();
    postgres::adopt(&config, &id).unwrap();
    let manifest = root.join("config.json");
    durable::write_json(&manifest, &config).unwrap();
    let mut postmaster = Command::new(env!("CARGO_BIN_EXE_harbor-db-postgres"))
        .args(["--config", manifest.to_str().unwrap(), "serve"])
        .env("PGDATA", root.join("wrong"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    let guard = Stop {
        package: &package,
        data: &data,
    };
    let start = Instant::now();
    loop {
        if pg_core::inspect_live(&config, &id, &socket, 55440).is_ok() {
            break;
        }
        assert!(
            postmaster.try_wait().unwrap().is_none(),
            "guarded writer exited"
        );
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(30));
    }
    let pid = fs::read_to_string(data.join("postmaster.pid"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(pid, postmaster.id());
    assert!(durable::lock(&state.join("lock"), false, false).is_err());
    let endpoint = vec![
        "-h".into(),
        socket.display().to_string(),
        "-p".into(),
        "55440".into(),
        "-U".into(),
        "postgres".into(),
        "-d".into(),
        "postgres".into(),
    ];
    let mut sql = endpoint.clone();
    sql.extend([
        "-c".into(),
        "CREATE TABLE records(id int,revision bigint);INSERT INTO records VALUES(1,7)".into(),
    ]);
    run("psql", sql);
    let mut legacy_quoting = endpoint.clone();
    legacy_quoting.extend(["-c".into(), "ALTER DATABASE postgres SET standard_conforming_strings = off; ALTER DATABASE postgres SET TimeZone = 'Pacific/Auckland'; ALTER DATABASE postgres SET DateStyle = 'SQL,DMY'; ALTER DATABASE postgres SET bytea_output = 'escape'".into()]);
    run("psql", legacy_quoting);
    // Every configured check must retain the read-only boundary, in both engines.
    // This is a disposable real primary, so also inspect its rows after rejected SQL.
    let attacks = [
        "COMMIT; DELETE FROM records; SELECT 1",
        "ROLLBACK; DELETE FROM records; SELECT 1",
        "END; DELETE FROM records; SELECT 1",
        "SELECT 1; COMMIT; DELETE FROM records; SELECT 1",
        "/* outer /* nested */ comment */ COMMIT; DELETE FROM records",
        "SET TRANSACTION READ WRITE; DELETE FROM records; SELECT 1",
        "WITH removed AS (DELETE FROM records RETURNING id) SELECT id FROM removed",
        "DO $$ BEGIN COMMIT; DELETE FROM records; END $$",
        "SELECT 1; SELECT 2",
        "COMMIT",
        "-- leading comment\nCOMMIT",
        "SELECT set_config('transaction_read_only','off',true)",
        "SELECT E'continued'\n'\\'';COMMIT;DELETE FROM records;--'",
        "\\! echo escaped-client-command",
        "SELECT 1; -- misleading tail\nROLLBACK; DELETE FROM records",
        "PREPARE TRANSACTION 'escape'",
    ];
    let positive = [
        ("SELECT id,revision FROM records ORDER BY id;", "1|7\n"),
        (
            "/* outer /* nested */ comment */ SELECT ';COMMIT;DELETE' -- tail",
            ";COMMIT;DELETE\n",
        ),
        ("SELECT $tag$;COMMIT;DELETE$tag$;", ";COMMIT;DELETE\n"),
        (
            "SELECT E'escaped\\';COMMIT;DELETE';",
            "escaped';COMMIT;DELETE\n",
        ),
        (
            "WITH retained AS (SELECT id FROM records) SELECT id FROM retained;",
            "1\n",
        ),
        ("SELECT true", "t\n"),
        (
            "SELECT '2000-01-02 03:04:05'::timestamptz,decode('42','hex')",
            "2000-01-02 03:04:05+00|\\x42\n",
        ),
        ("SELECT 1 AS \"semi;colon\"", "1\n"),
        (
            "SELECT E'continued'\n'\\';COMMIT;DELETE'",
            "continued';COMMIT;DELETE\n",
        ),
        (
            r"SELECT '\'';COMMIT;DELETE FROM records;--'",
            "\\';COMMIT;DELETE FROM records;--\n",
        ),
    ];
    let mut rejections = Vec::new();
    for native in [true, false] {
        let query = |statement: &str| {
            if native {
                harbor_db::storage::recovery::query(&config, &socket, 55440, "postgres", statement)
                    .map_err(|e| e.to_string())
            } else {
                let mut spec = CommandSpec::new(vec![
                    "python3".into(), "-B".into(), "-c".into(),
                    "import json,sys;from harbor_db import recovery;print(recovery.query(json.loads(sys.argv[1]),sys.argv[2],55440,'postgres',sys.argv[3]),end='')".into(),
                    config.to_string(), socket.display().to_string(), statement.into(),
                ]);
                spec.environment = Some(std::collections::BTreeMap::from([
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                ]));
                process::execute(&spec)
                    .and_then(|result| process::text(&result))
                    .map_err(|e| e.to_string())
            }
        };
        assert_eq!(
            query("SELECT current_setting('default_transaction_read_only'),current_setting('transaction_read_only')").unwrap(),
            "off|on\n",
            "native={native}: recovery inspection must observe the endpoint default while keeping its validation transaction read-only"
        );
        for statement in attacks {
            let rejected = query(statement).is_err();
            let mut inspect = endpoint.clone();
            inspect.extend([
                "-XAt".into(),
                "-c".into(),
                "SELECT id,revision FROM records ORDER BY id".into(),
            ]);
            let retained = run("psql", inspect).trim() == "1|7";
            rejections.push((native, statement, rejected, retained));
            if !retained {
                let mut reset = endpoint.clone();
                reset.extend([
                    "-c".into(),
                    "DELETE FROM records; INSERT INTO records VALUES(1,7)".into(),
                ]);
                run("psql", reset);
            }
        }
        for (statement, expected) in positive {
            let actual = query(statement)
                .unwrap_or_else(|error| panic!("native={native}, sql={statement:?}: {error}"));
            assert_eq!(actual, expected, "native={native}, sql={statement}");
        }
    }
    assert!(
        rejections
            .iter()
            .all(|(_, _, rejected, retained)| *rejected && *retained),
        "{rejections:?}"
    );
    let namespace = tempfile::tempdir().unwrap();
    // Keep the real primary outside the user namespace. A private passwd corpus
    // maps the client worker's postgres identity to the sole non-root uid; production
    // worker identity checks and JSON custody admission still execute unchanged.
    let launch = r#"import os,subprocess,sys
from pathlib import Path
sys.stderr=sys.stdout
root=Path(sys.argv[1]);source=Path(sys.argv[2]);corpus=Path(sys.argv[3]);binary=sys.argv[4]
# An external Cargo target directory is outside the mounted source tree.
# Retain the exact current test executable for its namespace continuation.
paths=[Path('/nix'),Path('/proc'),Path('/dev'),source,corpus,Path(binary).parent]
# Registrations without F resolve the interpreter again inside this chroot.
if Path('/run/binfmt').is_dir(): paths.append(Path('/run/binfmt'))
for path in paths:
    target=root/path.relative_to('/');target.mkdir(parents=True,exist_ok=True)
    subprocess.run(['mount','--rbind',str(path),str(target)],check=True,stderr=subprocess.STDOUT)
    subprocess.run(['mountpoint','-q',str(target)],check=True,stderr=subprocess.STDOUT)
(root/'tmp').mkdir(mode=0o1777);(root/'etc').mkdir()
(root/'etc/passwd').write_text('postgres:x:71:71:fixture:/tmp:/bin/sh\n')
(root/'etc/group').write_text('postgres:x:71:\n')
os.chroot(root);os.chdir(source);os.environ['TMPDIR']='/tmp';os.environ['HARBOR_DB_CUSTODY_SQL_CHILD']=str(corpus)
os.execv(binary,[binary,'--exact','native_postmaster_retains_main_pid_lease_and_logical_restore_is_private','--nocapture'])
"#;
    let mut custody = CommandSpec::new(vec![
        "unshare".into(),
        "--user".into(),
        "--map-user=71".into(),
        "--map-group=71".into(),
        "--keep-caps".into(),
        "--mount".into(),
        "--".into(),
        "python3".into(),
        "-B".into(),
        "-c".into(),
        launch.into(),
        namespace.path().display().to_string(),
        env!("CARGO_MANIFEST_DIR").into(),
        root.display().to_string(),
        std::env::current_exe().unwrap().display().to_string(),
    ]);
    custody.timeout = Duration::from_secs(90);
    let output = process::run(&custody).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let mut dump = endpoint;
    dump.extend([
        "--format=custom".into(),
        "--file".into(),
        backup.join("database.dump").display().to_string(),
    ]);
    run("pg_dump", dump);
    drop(guard);
    assert!(postmaster.wait().unwrap().success());
    durable::lock(&state.join("lock"), false, false).unwrap();
    let restored =
        postgres_drill::operate(&package, "restore", &backup, &workspace, "database.dump");
    assert!(restored.is_ok(), "{restored:?}");
    let result = run(
        "psql",
        vec![
            "-X".into(),
            "-At".into(),
            "-h".into(),
            workspace.join("socket").display().to_string(),
            "-p".into(),
            "55439".into(),
            "-d".into(),
            "harbor_restore".into(),
            "-c".into(),
            "SELECT id,revision FROM records".into(),
        ],
    );
    assert_eq!(result.trim(), "1|7");
    assert!(
        postgres_drill::operate(&package, "restore", &backup, &workspace, "database.dump").is_err()
    );
    postgres_drill::operate(&package, "cleanup", &backup, &workspace, "database.dump").unwrap();
    assert!(!workspace.join("cluster/postmaster.pid").exists());
    postgres_drill::operate(&package, "cleanup", &backup, &workspace, "database.dump").unwrap();

    // Exercise actual cross-major copy upgrade, rather than a synthetic pg_upgrade.
    let old_package = PathBuf::from(
        std::env::var("HARBOR_DB_TEST_POSTGRES_17")
            .expect("an explicit disposable PG17 qualification package is required"),
    );
    assert!(
        old_package.join("bin/pg_upgrade").is_file(),
        "PG17 qualification package missing; set HARBOR_DB_TEST_POSTGRES_17"
    );
    let old_data = root.join("old17");
    let upgrade_state = root.join("upgrade-state");
    fs::create_dir(&upgrade_state).unwrap();
    let old_run = |program: &str, args: Vec<String>| {
        let mut spec = pg_core::command(
            std::iter::once(old_package.join("bin").join(program).display().to_string())
                .chain(args)
                .collect(),
            true,
        );
        spec.timeout = Duration::from_secs(30);
        let output = process::run(&spec).unwrap();
        assert!(output.status.success(), "old {program} failed");
    };
    old_run(
        "initdb",
        vec![
            "-D".into(),
            old_data.display().to_string(),
            "-U".into(),
            "postgres".into(),
            "--locale=C".into(),
            "--encoding=UTF8".into(),
            "--auth=trust".into(),
            "--data-checksums".into(),
        ],
    );
    old_run(
        "pg_ctl",
        vec![
            "-D".into(),
            old_data.display().to_string(),
            "-l".into(),
            "/dev/null".into(),
            "-o".into(),
            format!("-k {} -p 55441 -c listen_addresses=", socket.display()),
            "-w".into(),
            "start".into(),
        ],
    );
    let old_guard = Stop {
        package: &old_package,
        data: &old_data,
    };
    old_run("psql",vec!["-h".into(),socket.display().to_string(),"-p".into(),"55441".into(),"-U".into(),"postgres".into(),"-d".into(),"postgres".into(),"-c".into(),"CREATE TABLE upgrade_records(id int,revision bigint);INSERT INTO upgrade_records VALUES(4,19)".into()]);
    drop(old_guard);
    let source = json!({"resource":"upgrade","major":"17","package":old_package,"state_dir":upgrade_state,"data_dir":old_data});
    let old_id = pg_core::inspect_cluster(&old_package, &old_data, "17").unwrap();
    postgres::adopt(&source, &old_id).unwrap();
    let before = pg_core::control_digest(&old_data).unwrap();
    // Explicit source data_directory in copied config must be overridden safely.
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(old_data.join("postgresql.conf"))
            .unwrap(),
        "\ndata_directory = '{}'",
        old_data.display()
    )
    .unwrap();
    fs::write(
        old_data.join("postgresql.auto.conf"),
        format!("data_directory = '{}'\n", old_data.display()),
    )
    .unwrap();
    // NixOS configuration links are dereferenced only inside the disposable copy.
    let external_config = root.join("source-postgresql.conf");
    fs::rename(old_data.join("postgresql.conf"), &external_config).unwrap();
    symlink(&external_config, old_data.join("postgresql.conf")).unwrap();
    let source_config = fs::read(&external_config).unwrap();
    let validator = root.join("validate-upgrade");
    executable(
        &validator,
        &format!(
            "set -eu\n'{}' -D \"$1\" -l /dev/null -o \"-k {} -p 55442 -c listen_addresses= -c data_directory='$1'\" -w start\ntrap '\"{}\" -D \"$1\" -m fast -w stop >/dev/null' EXIT\nresult=$('{}' -X -At -h '{}' -p 55442 -U postgres -d postgres -c 'SELECT id,revision FROM upgrade_records')\ntest \"$result\" = '4|19'",
            package.join("bin/pg_ctl").display(),
            socket.display(),
            package.join("bin/pg_ctl").display(),
            package.join("bin/psql").display(),
            socket.display()
        ),
    );
    let target = root.join("new18");
    let target_config = json!({"resource":"upgrade","major":"18","package":package,"state_dir":upgrade_state,"data_dir":target,"upgrade":{"data_dir":old_data,"major":"17","package":old_package,"initdb_args":["-U","postgres","--locale=C","--encoding=UTF8","--auth=trust"],"validate_command":[validator]}});
    let upgraded = postgres::upgrade(&target_config, false);
    if upgraded.is_err() {
        fn diagnostics(path: &Path) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        diagnostics(&path);
                    } else if path.extension().is_some_and(|e| e == "log" || e == "txt")
                        && let Ok(text) = fs::read_to_string(&path)
                    {
                        println!("{}:\n{text}", path.display());
                    }
                }
            }
        }
        diagnostics(&root.join("new18.harbor-staging"));
    }
    upgraded.unwrap();
    assert_eq!(pg_core::control_digest(&old_data).unwrap(), before);
    assert!(
        fs::symlink_metadata(old_data.join("postgresql.conf"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&external_config).unwrap(), source_config);
    assert!(
        !fs::symlink_metadata(root.join("new18.harbor-source/postgresql.conf"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(old_data.exists());
    assert!(root.join("new18.harbor-source").exists());
    assert!(!upgrade_state.join("upgrade.json").exists());
    postgres::check(&target_config).unwrap();
    assert!(postgres::check(&source).is_err());
    postgres::upgrade(&target_config, false).unwrap();
}

fn custody_sql_boundary(root: &std::path::Path) {
    use harbor_db::storage::{application_transition, process};
    let corpus = root.join("custody-corpus");
    fs::create_dir(&corpus).unwrap();
    fs::write(corpus.join("records"), "custody corpus").unwrap();
    let target = json!({"resource":"native","state_dir":root.join("custody-authority"),"directories":[corpus],"binding":{"backend":"files"},"required_mounts":[]});
    let entry = json!({"kind":"filesystem","authority":target,"custody_file":root.join("custody.json"),"database_inventory_checks":[]});
    let selected = root.join("custody-manifest.json");
    let config = json!({"custody_manifest":selected,"postgres_manifest":root.join("config.json"),"postgres_socket":root.join("socket"),"postgres_port":55440,"timeout_seconds":30,"units":[],"retired_units":[]});
    let record = json!({"source_authority":{"identity":"fixture"},"primary_snapshot_sha256":null,"source_acceptance_sha256":null,"independent_sha256":null,"semantic_sha256":null});
    let database = durable::read_json(&root.join("config.json")).unwrap();
    let sql = |statement: &str| {
        let spec = process::CommandSpec::new(vec![
            format!("{}/bin/psql", database["package"].as_str().unwrap()),
            "-XqAt".into(),
            "-h".into(),
            root.join("socket").display().to_string(),
            "-p".into(),
            "55440".into(),
            "-U".into(),
            "postgres".into(),
            "-d".into(),
            "postgres".into(),
            "-c".into(),
            statement.into(),
        ]);
        String::from_utf8(process::execute(&spec).unwrap()).unwrap()
    };
    let mut outcomes = Vec::new();
    for native in [true, false] {
        for statement in [
            "COMMIT; DELETE FROM records; SELECT '[]'::json",
            "ROLLBACK; DELETE FROM records; SELECT '[]'::json",
            "SELECT '[]'::json; COMMIT; DELETE FROM records; SELECT '[]'::json",
            "WITH removed AS (DELETE FROM records RETURNING id) SELECT '[]'::json",
            "SELECT '[]'::json; SELECT '[]'::json",
        ] {
            let mut check = entry.clone();
            check["database_inventory_checks"] =
                json!([{"root":corpus,"database":"postgres","sql":statement}]);
            durable::write_json(&selected, &check).unwrap();
            let rejected = if native {
                application_transition::target_custody(&config, &record, &target, &[]).is_err()
            } else {
                let mut spec = process::CommandSpec::new(vec!["python3".into(),"-B".into(),"-c".into(),"import json,sys;from harbor_db import application_transition;print(json.dumps(application_transition.target_custody(*map(json.loads,sys.argv[1:]),())))".into(),config.to_string(),record.to_string(),target.to_string()]);
                spec.environment = Some(std::collections::BTreeMap::from([
                    (
                        "PYTHONPATH".into(),
                        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                    ),
                    ("PATH".into(), std::env::var("PATH").unwrap()),
                ]));
                process::execute(&spec).is_err()
            };
            let retained = sql("SELECT id,revision FROM records ORDER BY id").trim() == "1|7";
            outcomes.push((native, statement, rejected, retained));
            if !retained {
                sql("DELETE FROM records; INSERT INTO records VALUES(1,7)");
            }
        }
        let mut check = entry.clone();
        check["database_inventory_checks"] = json!([{"root":corpus,"database":"postgres","sql":"/* nested /* comment */ end */ SELECT $tag$[]$tag$::json; -- quoted boundary"}]);
        durable::write_json(&selected, &check).unwrap();
        if native {
            assert_eq!(
                application_transition::target_custody(&config, &record, &target, &[]).unwrap()["database_requirements"],
                json!([])
            );
        } else {
            let mut spec=process::CommandSpec::new(vec!["python3".into(),"-B".into(),"-c".into(),"import json,sys;sys.stderr=sys.stdout;from harbor_db import application_transition;r=application_transition.target_custody(*map(json.loads,sys.argv[1:]),());assert r['database_requirements']==[]".into(),config.to_string(),record.to_string(),target.to_string()]);
            spec.environment = Some(std::collections::BTreeMap::from([
                (
                    "PYTHONPATH".into(),
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                ),
                ("PATH".into(), std::env::var("PATH").unwrap()),
            ]));
            let result = process::run(&spec).unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
    }
    assert!(
        outcomes
            .iter()
            .all(|(_, _, rejected, retained)| *rejected && *retained),
        "{outcomes:?}"
    );
}

#[test]
fn missing_cluster_never_initializes_or_adopts() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let config = json!({"resource":"db","major":"18","package":dir.path(),"state_dir":state,"data_dir":dir.path().join("missing")});
    assert!(postgres::adopt(&config, "123").is_err());
    assert!(!state.join("identity.json").exists());
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn configuration_rejects_redirected_and_nested_authority() {
    let dir = tempfile::tempdir().unwrap();
    symlink(dir.path(), dir.path().join("redirect")).unwrap();
    let mut config = json!({"resource":"db","major":18,"package":dir.path(),"state_dir":dir.path().join("state"),"data_dir":dir.path().join("data")});
    pg_core::validate_config(&config).unwrap();
    config["state_dir"] = json!(dir.path().join("data/state"));
    assert!(pg_core::validate_config(&config).is_err());
    config["state_dir"] = json!(dir.path().join("redirect/state"));
    assert!(pg_core::validate_config(&config).is_err());
}

#[test]
fn registered_identity_binds_generation_and_upgrade_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({"resource":"db","major":18,"package":dir.path(),"state_dir":dir.path(),"data_dir":dir.path().join("data")});
    durable::write_json(
        &dir.path().join("identity.json"),
        &pg_core::identity(&config, "123").unwrap(),
    )
    .unwrap();
    pg_core::registered(&config).unwrap();
    let mut stale = config.clone();
    stale["major"] = json!(17);
    assert!(pg_core::registered(&stale).is_err());
    durable::write_json(
        &dir.path().join("upgrade.json"),
        &json!({"phase":"building"}),
    )
    .unwrap();
    assert!(pg_core::reject_upgrade(&config).is_err());
}
