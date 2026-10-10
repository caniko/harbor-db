use harbor_db::storage::{backup, codec, durable, process};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    fs::FileTimes,
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, SystemTime},
};

const SEGMENT: u64 = 1024 * 1024;
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for dir in ["base", "wal", "locks", "recovery/pins"] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    for (path, bytes) in [
        ("locks/mutate", ""),
        ("BACKUP_LOCK", ""),
        ("LAST_SUCCESS", "legacy\n"),
        ("recovery/PROTOCOL", "source-local-v1\n"),
    ] {
        fs::write(root.path().join(path), bytes).unwrap();
    }
    for (index, name) in ["old", "expired", "new", "latest"].iter().enumerate() {
        let dir = root.path().join("base").join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("data"), [0, 255, index as u8, 42]).unwrap();
        fs::write(dir.join("backup_manifest"), json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":format!("0/{:X}", (index + 2) * SEGMENT as usize),"End-LSN":format!("0/{:X}", (index + 3) * SEGMENT as usize)}]}).to_string()).unwrap();
        fs::File::open(dir)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100 + index as u64)),
            )
            .unwrap();
    }
    for n in 1..=6 {
        let path = root.path().join(format!("wal/0000000100000000{n:08X}"));
        fs::write(&path, vec![n as u8; SEGMENT as usize]).unwrap();
        fs::File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    root
}
fn payload(root: &Path) -> Value {
    json!({"version":1,"capture_id":"capture-1","backup_id":"old","wal_segment_bytes":SEGMENT,"manifest_sha256":codec::digest(&fs::read(root.join("base/old/backup_manifest")).unwrap()),"pg_major":17,"system_identifier":"123456789","epoch_id":"fence-1","writer_fence_token":"fence-1","record_contract_sha256":"1".repeat(64),"record_hashes":{"records":"2".repeat(64)},"backup_stop_lsn":"0/300000","post_backup_lsn":"0/400000","completed_at":100})
}
fn pin(root: &Path) {
    let temporary = root.join("recovery/pin.tmp");
    fs::write(&temporary, payload(root).to_string()).unwrap();
    durable::publish_file(&temporary, &root.join("recovery/pins/capture-1.json")).unwrap();
}
fn python(root: &Path, script: &str) -> Output {
    let mut command = Command::new("python3");
    command
        .args(["-B", "-c", script])
        .arg(root)
        .env(
            "PYTHONPATH",
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}
const PRUNE: &str =
    "import sys; from harbor_db.backup import prune; prune(sys.argv[1],1,2,1048576,now=1000000)";
fn inventory(root: &Path) -> BTreeMap<String, (u64, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<String, (u64, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                fs::read(&path).unwrap()
            } else if meta.file_type().is_symlink() {
                fs::read_link(&path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            result.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                (meta.ino(), bytes),
            );
            if meta.is_dir() {
                visit(root, &path, result);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
#[test]
fn cross_carrier_pins_preserve_old_backup_and_actual_wal_floor() {
    let roots = [fixture(), fixture()];
    let produced = python(
        roots[0].path(),
        "import sys,json,hashlib; from pathlib import Path; from harbor_db.durable import publish_file; r=Path(sys.argv[1]); p={'version':1,'capture_id':'capture-1','backup_id':'old','wal_segment_bytes':1048576,'manifest_sha256':hashlib.sha256((r/'base/old/backup_manifest').read_bytes()).hexdigest(),'pg_major':17,'system_identifier':'123456789','epoch_id':'fence-1','writer_fence_token':'fence-1','record_contract_sha256':'1'*64,'record_hashes':{'records':'2'*64},'backup_stop_lsn':'0/300000','post_backup_lsn':'0/400000','completed_at':100}; t=r/'recovery/pin.tmp'; t.write_text(json.dumps(p,sort_keys=True,separators=(',',':'))); publish_file(t,r/'recovery/pins/capture-1.json')",
    );
    assert!(produced.status.success(), "{:?}", produced);
    pin(roots[1].path());
    for (index, root) in roots.iter().enumerate() {
        let before = inventory(root.path());
        if index == 0 {
            backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).unwrap();
        } else {
            assert!(python(root.path(), PRUNE).status.success());
        }
        assert!(root.path().join("base/old/data").exists());
        assert!(!root.path().join("base/expired").exists());
        assert!(!root.path().join("wal/000000010000000000000001").exists());
        assert!(root.path().join("wal/000000010000000000000002").exists());
        let after = inventory(root.path());
        for (name, state) in &after {
            assert_eq!(before.get(name), Some(state), "{name}");
        }
        backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).unwrap();
        assert!(python(root.path(), PRUNE).status.success());
        assert_eq!(inventory(root.path()), after);
    }
    let content = |root: &Path| {
        inventory(root)
            .into_iter()
            .map(|(p, (_, b))| (p, b))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(content(roots[0].path()), content(roots[1].path()));
}
#[test]
fn uncertainty_preserves_entire_tree_in_both_carriers() {
    for case in 0..24 {
        let mut outcomes = Vec::new();
        for native in [true, false] {
            let root = fixture();
            pin(root.path());
            let pin_path = root.path().join("recovery/pins/capture-1.json");
            let mut value = payload(root.path());
            match case {
                0 => {
                    value["version"] = json!(2);
                }
                1 => {
                    value["capture_id"] = json!("other");
                }
                2 => {
                    value["backup_id"] = json!("../old");
                }
                3 => {
                    value["manifest_sha256"] = json!("0".repeat(64));
                }
                4 => {
                    value["wal_segment_bytes"] = json!(2 * SEGMENT);
                }
                5 => {
                    fs::remove_dir_all(root.path().join("base/old")).unwrap();
                }
                6 => {
                    fs::write(root.path().join("recovery/PROTOCOL"), "wrong").unwrap();
                }
                7 => {
                    fs::remove_file(root.path().join("recovery/PROTOCOL")).unwrap();
                }
                8 => {
                    fs::write(root.path().join("recovery/pins/.temporary"), "partial").unwrap();
                }
                9 => {
                    fs::write(root.path().join("recovery/pins/unknown"), "unknown").unwrap();
                }
                10 => {
                    fs::remove_file(&pin_path).unwrap();
                    std::os::unix::fs::symlink(root.path().join("LAST_SUCCESS"), &pin_path)
                        .unwrap();
                }
                11 => {
                    fs::remove_file(root.path().join("base/old/backup_manifest")).unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("base/new/backup_manifest"),
                        root.path().join("base/old/backup_manifest"),
                    )
                    .unwrap();
                }
                12 => {
                    fs::remove_file(root.path().join("locks/mutate")).unwrap();
                }
                13 => {
                    fs::remove_dir_all(root.path().join("recovery/pins")).unwrap();
                }
                14 => {
                    fs::remove_file(root.path().join("recovery/PROTOCOL")).unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("LAST_SUCCESS"),
                        root.path().join("recovery/PROTOCOL"),
                    )
                    .unwrap();
                }
                15 => {
                    fs::rename(
                        &pin_path,
                        root.path().join("recovery/pins/capture.partial.json"),
                    )
                    .unwrap();
                }
                16 => {
                    fs::write(&pin_path, "{").unwrap();
                }
                17 => {
                    fs::rename(
                        root.path().join("recovery/pins"),
                        root.path().join("recovery/redirected-pins"),
                    )
                    .unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("recovery/redirected-pins"),
                        root.path().join("recovery/pins"),
                    )
                    .unwrap();
                }
                18 => {
                    fs::rename(
                        root.path().join("recovery"),
                        root.path().join("redirected-recovery"),
                    )
                    .unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("redirected-recovery"),
                        root.path().join("recovery"),
                    )
                    .unwrap();
                }
                19 => {
                    fs::remove_file(root.path().join("locks/mutate")).unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("BACKUP_LOCK"),
                        root.path().join("locks/mutate"),
                    )
                    .unwrap();
                }
                20 => {
                    fs::rename(
                        root.path().join("base/old"),
                        root.path().join("base/redirected-old"),
                    )
                    .unwrap();
                    std::os::unix::fs::symlink(
                        root.path().join("base/redirected-old"),
                        root.path().join("base/old"),
                    )
                    .unwrap();
                }
                21 => {
                    fs::rename(
                        &pin_path,
                        root.path()
                            .join(format!("recovery/pins/{}.json", "a".repeat(129))),
                    )
                    .unwrap();
                }
                22 => {
                    fs::rename(&pin_path, root.path().join("recovery/pins/capturé.json")).unwrap();
                }
                23 => {
                    fs::remove_file(&pin_path).unwrap();
                    fs::create_dir(&pin_path).unwrap();
                }
                _ => unreachable!(),
            }
            if case < 5 {
                fs::write(&pin_path, value.to_string()).unwrap();
            }
            let before = inventory(root.path());
            outcomes.push(if native {
                backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).is_err()
            } else {
                !python(root.path(), PRUNE).status.success()
            });
            assert_eq!(
                inventory(root.path()),
                before,
                "case {case}, native {native}"
            );
        }
        assert_eq!(outcomes, vec![true, true], "case {case}");
    }
}
#[test]
fn shared_mutation_lease_rejects_native_and_python_cli_with_eagain() {
    let root = fixture();
    pin(root.path());
    let _lease = durable::lock(&root.path().join("locks/mutate"), true, false).unwrap();
    let before = inventory(root.path());
    let error = backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).unwrap_err();
    assert!(
        matches!(&error, harbor_db::storage::StorageError::Io(e) if e.raw_os_error() == Some(libc::EAGAIN))
    );
    assert!(
        error.to_string().contains("temporarily unavailable"),
        "{error}"
    );
    let mut native = Command::new(env!("CARGO_BIN_EXE_harbor-db-backup-prune"));
    native
        .arg("--root")
        .arg(root.path())
        .args([
            "--base-days",
            "1",
            "--wal-days",
            "2",
            "--segment-bytes",
            "1048576",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = process::spawn(&mut native)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("temporarily unavailable"),
        "{:?}",
        output
    );
    let output = python(
        root.path(),
        "import sys; from harbor_db.backup import main; sys.argv=['prune','--root',sys.argv[1],'--base-days','1','--wal-days','2','--segment-bytes','1048576']; sys.exit(main())",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("temporarily unavailable"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Errno 11"));
    assert_eq!(inventory(root.path()), before);
}
#[test]
fn source_local_requires_absolute_unredirected_root() {
    let root = fixture();
    pin(root.path());
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("root");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    let relative = root.path().strip_prefix("/").unwrap();
    let before = inventory(root.path());
    assert!(backup::prune(&alias, 1, 2, SEGMENT, Some(1_000_000.0)).is_err());
    assert!(!python(&alias, PRUNE).status.success());
    // A root-relative path exists from / but is still not an absolute carrier.
    let mut native = Command::new(env!("CARGO_BIN_EXE_harbor-db-backup-prune"));
    native
        .current_dir("/")
        .arg("--root")
        .arg(relative)
        .args([
            "--base-days",
            "1",
            "--wal-days",
            "2",
            "--segment-bytes",
            "1048576",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    assert!(
        !process::spawn(&mut native)
            .unwrap()
            .wait_with_output()
            .unwrap()
            .status
            .success()
    );
    assert!(!python(relative, "import os,sys; os.chdir('/'); from harbor_db.backup import prune; prune(sys.argv[1],1,2,1048576,now=1000000)").status.success());
    assert_eq!(inventory(root.path()), before);
}
#[test]
fn empty_valid_pins_use_normal_retention() {
    for native in [true, false] {
        let root = fixture();
        if native {
            backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).unwrap();
        } else {
            assert!(python(root.path(), PRUNE).status.success());
        }
        assert!(!root.path().join("base/old").exists());
        assert!(!root.path().join("base/expired").exists());
    }
}

#[test]
fn large_pinned_manifest_prunes_in_both_carriers_without_releasing_its_backup() {
    for native in [true, false] {
        let root = fixture();
        let path = root.path().join("base/old/backup_manifest");
        let mut manifest = fs::read(&path).unwrap();
        manifest.resize(17 * 1024 * 1024, b' ');
        fs::write(&path, manifest).unwrap();
        pin(root.path());
        let before = inventory(root.path());
        if native {
            backup::prune(root.path(), 1, 2, SEGMENT, Some(1_000_000.0)).unwrap();
        } else {
            let output = python(root.path(), PRUNE);
            assert!(output.status.success(), "{output:?}");
        }
        assert!(root.path().join("base/old/data").exists());
        assert!(!root.path().join("base/expired").exists());
        assert!(root.path().join("wal/000000010000000000000002").exists());
        for (path, state) in inventory(root.path()) {
            assert_eq!(before.get(&path), Some(&state), "{path}");
        }
    }
}
