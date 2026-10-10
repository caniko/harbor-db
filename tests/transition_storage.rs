use harbor_db::storage::{application_transition, durable, process, transition_manifest};
use serde_json::json;
use std::{fs, os::unix::fs::symlink};

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
        serde_json::from_slice::<serde_json::Value>(&process::execute(&command).unwrap()).unwrap()
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
        record["retained_obligation"] =
            json!({"unknown_future_key":"preserved ☃", "generation":42});
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
