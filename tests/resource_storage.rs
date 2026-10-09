use harbor_db::storage::durable;
use harbor_db::storage::resource;
use serde_json::Value;
use serde_json::json;
use std::{fs, path::Path};
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
    c["consumer_command"] = json!(["cat", t.join("consumer.json")]);
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
    c["consumer_command"] = json!([
        "/bin/sh",
        "-c",
        "printf 'contract rejected\r\n' >&2; exit 1"
    ]);
    assert_eq!(
        resource::adopt(&c, "archive").unwrap_err().to_string(),
        "consumer storage validation failed: contract rejected"
    );
    assert!(!t.path().join("authority/identity.json").exists());
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
