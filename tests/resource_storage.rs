use harbor_db::storage::durable;
use harbor_db::storage::resource;
use serde_json::Value;
use serde_json::json;
use std::{fs, path::Path};

#[path = "fixtures/python_store.rs"]
mod python_store;

#[test]
fn consumer_snapshot_allows_advancing_evidence_but_rejects_namespace_changes_and_regressions_in_both_engines()
 {
    let (t, c) = fixture();
    let old = t.path().join("state/old");
    let new = t.path().join("state/new");
    fs::write(&old, b"retained").unwrap();
    fs::write(&new, b"new").unwrap();
    let pinned = json!({"version":1,"resource":"archive","binding":{},"directories":c["directories"],"required_files":[old],"minimum_counters":{"saved":2}});
    let mut advanced = pinned.clone();
    advanced["required_files"] = json!([old, new]);
    advanced["minimum_counters"]["saved"] = json!(3);
    resource::require_contract(&advanced, Some(&pinned)).unwrap();
    for field in [
        "binding",
        "directories",
        "required_files",
        "minimum_counters",
    ] {
        let mut regressed = advanced.clone();
        regressed[field] = match field {
            "binding" => json!({"changed":true}),
            "directories" => json!([]),
            "required_files" => json!([new]),
            _ => json!({"saved":1}),
        };
        assert!(resource::require_contract(&regressed, Some(&pinned)).is_err());
    }
    let script = r#"import json,sys
from harbor_db import resource
pinned,advanced=map(json.loads,sys.argv[1:])
resource.require_contract(advanced,pinned)
for field,value in [('binding',{'changed':True}),('directories',[]),('required_files',advanced['required_files'][1:]),('minimum_counters',{'saved':1})]:
    try: resource.require_contract(dict(advanced,**{field:value}),pinned)
    except ValueError: pass
    else: raise AssertionError('regressed snapshot admitted')
"#;
    let result = std::process::Command::new("python3")
        .args([
            "-B",
            "-c",
            script,
            &pinned.to_string(),
            &advanced.to_string(),
        ])
        .env(
            "PYTHONPATH",
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn fixture() -> (tempfile::TempDir, Value) {
    let t = tempfile::tempdir().unwrap();
    for d in ["authority", "data", "state"] {
        fs::create_dir(t.path().join(d)).unwrap();
    }
    let c = json!({"resource":"annotations","state_dir":t.path().join("authority"),"binding":{"backend":"filesystem","endpoint":t.path().join("data")},"directories":[t.path().join("data"),t.path().join("state")],"required_mounts":[]});
    (t, c)
}
fn consumer(t: &Path, c: &mut Value) -> Value {
    fs::write(t.join("state/review.toml"), "[sessions]\n").unwrap();
    let v = json!({"binding":{"dataset":"one","config_sha256":"verified"},"directories":[t.join("data")],"required_files":[t.join("state/review.toml")],"minimum_counters":{"revision":42,"review:one":1}});
    fs::write(t.join("consumer.json"), v.to_string()).unwrap();
    c["consumer_command"] = json!([
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("cat"))
            .find(|path| path.is_file())
            .unwrap(),
        t.join("consumer.json")
    ]);
    v
}

#[test]
fn python_and_rust_read_and_retry_each_others_authority_without_rewriting_receipts() {
    use harbor_db::storage::process;
    for python_first in [true, false] {
        let (t, c) = fixture();
        let script = "import json,sys; from harbor_db import resource; c=json.loads(sys.argv[1]); resource.adopt(c,'archive'); resource.check(c); print(json.dumps(resource.verify(c,resource.contract(c))))";
        let python = || {
            let mut command = process::CommandSpec::new(vec![
                "python3".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                c.to_string(),
            ]);
            command.environment = Some(std::collections::BTreeMap::from([
                (
                    "PYTHONPATH".into(),
                    format!("{}/python", env!("CARGO_MANIFEST_DIR")),
                ),
                ("PATH".into(), std::env::var("PATH").unwrap()),
            ]));
            serde_json::from_slice::<Value>(&process::execute(&command).unwrap()).unwrap()
        };
        let record = if python_first {
            python()
        } else {
            resource::adopt(&c, "archive").unwrap()
        };
        let receipt = t.path().join("authority/identity.json");
        let original = fs::read(&receipt).unwrap();
        resource::check(&c).unwrap();
        assert_eq!(python(), record);
        assert_eq!(resource::adopt(&c, "archive").unwrap(), record);
        assert_eq!(fs::read(&receipt).unwrap(), original);
    }
}
#[test]
fn startup_binding_missing_markers_and_required_files_fail_closed() {
    let (t, mut c) = fixture();
    assert!(resource::check(&c).is_err());
    assert_eq!(fs::read_dir(t.path().join("data")).unwrap().count(), 0);
    let file = t.path().join("state/state.json");
    fs::write(&file, "{\"revision\":42}").unwrap();
    c["required_files"] = json!([file]);
    resource::adopt(&c, "archive").unwrap();
    let mut changed = c.clone();
    changed["binding"] = json!({"backend":"postgresql","endpoint":"postgres:///other"});
    assert!(resource::check(&changed).is_err());
    assert!(resource::adopt(&changed, "archive").is_err());
    fs::remove_file(&file).unwrap();
    assert!(
        resource::check(&c)
            .unwrap_err()
            .to_string()
            .contains("required storage file")
    );
    fs::write(&file, "{}").unwrap();
    fs::remove_file(resource::anchor(&c, &t.path().join("state")).unwrap()).unwrap();
    assert!(resource::check(&c).is_err());
}
#[test]
fn adoption_preserves_unlinked_lock_and_retries_partial_markers() {
    let (t, c) = fixture();
    let marker = json!({"resource":"annotations","identity":"archive"});
    durable::write_json(
        &resource::anchor(&c, &t.path().join("data")).unwrap(),
        &marker,
    )
    .unwrap();
    assert!(resource::adopt(&c, "different").is_err());
    resource::adopt(&c, "archive").unwrap();
    resource::check(&c).unwrap();
    let lock = t.path().join("authority/lock");
    let _lease = durable::lock(&lock, true, false).unwrap();
    fs::remove_file(&lock).unwrap();
    assert!(resource::adopt(&c, "archive").is_err());
    assert!(!lock.exists());
}
#[test]
fn consumer_binding_floors_and_files_are_retained() {
    let (t, mut c) = fixture();
    let mut v = consumer(t.path(), &mut c);
    resource::adopt(&c, "archive").unwrap();
    for stale in [
        json!({"revision":41,"review:one":1}),
        json!({"revision":42}),
    ] {
        v["minimum_counters"] = stale;
        fs::write(t.path().join("consumer.json"), v.to_string()).unwrap();
        assert!(
            resource::check(&c)
                .unwrap_err()
                .to_string()
                .contains("older or incomplete")
        );
    }
    fs::write(t.path().join("state/later.toml"), "[sessions]\n").unwrap();
    v["required_files"]
        .as_array_mut()
        .unwrap()
        .push(json!(t.path().join("state/later.toml")));
    v["minimum_counters"] = json!({"revision":43,"review:one":1,"review:two":1});
    fs::write(t.path().join("consumer.json"), v.to_string()).unwrap();
    resource::check(&c).unwrap();
    v["binding"]["config_sha256"] = json!("stale");
    fs::write(t.path().join("consumer.json"), v.to_string()).unwrap();
    assert!(
        resource::check(&c)
            .unwrap_err()
            .to_string()
            .contains("authority mismatch")
    );
    v["binding"]["config_sha256"] = json!("verified");
    fs::write(t.path().join("consumer.json"), v.to_string()).unwrap();
    fs::remove_file(t.path().join("state/review.toml")).unwrap();
    assert!(
        resource::check(&c)
            .unwrap_err()
            .to_string()
            .contains("required storage file")
    );
}
#[test]
fn consumer_failure_preserves_diagnostics_and_does_not_publish() {
    let (t, mut c) = fixture();
    // Hosted Linux's /bin/sh is mutable; the validator requires a store executable.
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("sh"))
        .find(|path| path.starts_with("/nix/store") && path.is_file())
        .expect("immutable shell from the Nix development environment");
    c["consumer_command"] = json!([shell, "-c", "printf 'contract rejected\r\n' >&2; exit 1"]);
    assert_eq!(
        resource::adopt(&c, "archive").unwrap_err().to_string(),
        "consumer storage validation failed: contract rejected"
    );
    assert!(!t.path().join("authority/identity.json").exists());
}

#[test]
fn consumer_validators_reject_mutable_executables_before_dispatch_in_both_engines() {
    use harbor_db::storage::process;
    use std::os::unix::fs::PermissionsExt;
    let (t, mut c) = fixture();
    let executable = t.path().join("validator");
    let marker = t.path().join("dispatched");
    fs::write(
        &executable,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    c["consumer_command"] = json!([executable]);
    for program in [executable.clone(), std::path::PathBuf::from("sh")] {
        c["consumer_command"][0] = json!(program);
        assert!(
            resource::contract(&c).is_err(),
            "mutable consumer dispatched"
        );
        let script = "import json,sys; from harbor_db import resource; c=json.loads(sys.argv[1]);\ntry: resource.contract(c)\nexcept (OSError,ValueError): pass\nelse: raise AssertionError('mutable consumer dispatched')";
        let mut spec = process::CommandSpec::new(vec![
            "python3".into(),
            "-B".into(),
            "-c".into(),
            python_store::script(script),
            c.to_string(),
        ]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        process::execute(&spec).unwrap();
        assert!(!marker.exists());
        assert!(!t.path().join("authority/identity.json").exists());
    }
}
#[test]
fn transition_barriers_and_redirects_are_rejected() {
    let (t, mut c) = fixture();
    resource::adopt(&c, "archive").unwrap();
    for phase in ["planned", "write-enabled", "complete", "aborted"] {
        durable::write_json(
            &t.path().join("authority/transition.json"),
            &json!({"phase":phase}),
        )
        .unwrap();
        resource::check(&c).unwrap();
    }
    durable::write_json(
        &t.path().join("authority/transition.json"),
        &json!({"phase":"restoring"}),
    )
    .unwrap();
    assert!(resource::check(&c).is_err());
    fs::remove_file(t.path().join("authority/transition.json")).unwrap();
    std::os::unix::fs::symlink(t.path().join("data"), t.path().join("redirect")).unwrap();
    c["directories"] = json!([t.path().join("redirect")]);
    assert!(resource::contract(&c).is_err());
    c["directories"] = json!([t.path()]);
    assert!(resource::contract(&c).is_err());
}

#[test]
fn consumer_limits_reject_large_output_and_deadlines_and_release_authority_in_both_engines() {
    use harbor_db::storage::process;
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .unwrap();
    for (code, expected) in [
        (
            "import sys; sys.stdout.write(' ' * (1024*1024+1)); print('{\"binding\":{},\"directories\":[],\"required_files\":[]}')",
            "size limit",
        ),
        (
            "import sys; sys.stderr.write('x' * (1024*1024+1)); print('{\"binding\":{},\"directories\":[],\"required_files\":[]}')",
            "size limit",
        ),
        (
            "import time; time.sleep(2); print('{\"binding\":{},\"directories\":[],\"required_files\":[]}')",
            "execution limit",
        ),
    ] {
        let (t, mut c) = fixture();
        resource::adopt(&c, "archive").unwrap();
        c["consumer_command"] = json!([python, "-B", "-c", code]);
        c["consumer_timeout_seconds"] = json!(1);
        let before = fs::read(t.path().join("authority/identity.json")).unwrap();
        let error = resource::inspection(&c)
            .err()
            .expect("unbounded validator accepted")
            .to_string();
        assert!(error.contains(expected), "{error}");
        let script = "import json,sys; from harbor_db import resource; c=json.loads(sys.argv[1]);\ntry:\n with resource.inspection(c): pass\nexcept ValueError as error:\n assert sys.argv[2] in str(error), str(error)\nelse: raise AssertionError('unbounded validator accepted')";
        let mut spec = process::CommandSpec::new(vec![
            python.to_string_lossy().into_owned(),
            "-B".into(),
            "-c".into(),
            python_store::script(script),
            c.to_string(),
            expected.into(),
        ]);
        spec.environment = Some(std::collections::BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]));
        process::execute(&spec).unwrap();
        durable::lock(&t.path().join("authority/lock"), false, false).unwrap();
        assert_eq!(
            fs::read(t.path().join("authority/identity.json")).unwrap(),
            before
        );
    }
}
#[test]
fn consumer_exec_retains_authority_fd_until_exit() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
    };
    let (t, c) = fixture();
    resource::adopt(&c, "archive").unwrap();
    let manifest = t.path().join("config.json");
    durable::write_json(&manifest, &c).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_harbor-db-resource"));
    command
        .args([
            "--config",
            manifest.to_str().unwrap(),
            "serve",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n'; read answer",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = harbor_db::storage::process::spawn(&mut command).unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    assert!(durable::lock(&t.path().join("authority/lock"), false, false).is_err());
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    durable::lock(&t.path().join("authority/lock"), false, false).unwrap();
}
#[test]
fn authority_cannot_be_replaced() {
    let t = tempfile::tempdir().unwrap();
    let state = t.path().join("state");
    let data = t.path().join("data");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&data).unwrap();
    let c = json!({"resource":"app","state_dir":state,"directories":[data],"binding":{"backend":"sqlite"}});
    assert!(resource::check(&c).is_err());
    resource::adopt(&c, "verified").unwrap();
    resource::check(&c).unwrap();
    assert!(resource::adopt(&c, "replacement").is_err());
    let mut e = resource::contract(&c).unwrap();
    let huge: serde_json::Value =
        serde_json::from_str("{\"revision\":184467440737095516160}").unwrap();
    let mut r = harbor_db::storage::durable::read_json(&state.join("identity.json")).unwrap();
    r["minimum_counters"] = huge.clone();
    harbor_db::storage::durable::write_json(&state.join("identity.json"), &r).unwrap();
    e["minimum_counters"] = huge;
    resource::verify(&c, &e).unwrap();
    e["minimum_counters"]["revision"] = json!(18446744073709551615u64);
    assert!(resource::verify(&c, &e).is_err());
}
