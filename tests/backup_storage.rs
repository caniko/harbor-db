use harbor_db::storage::backup;
use serde_json::json;
use std::{
    fs,
    fs::FileTimes,
    time::{Duration, SystemTime},
};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("base")).unwrap();
    fs::create_dir(root.path().join("wal")).unwrap();
    for (index, name) in ["old", "middle", "new"].iter().enumerate() {
        let path = root.path().join("base").join(name);
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("backup_manifest"),
            json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]})
                .to_string(),
        )
        .unwrap();
        fs::File::open(path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100 + index as u64)),
            )
            .unwrap();
    }
    root
}

fn python_prune(root: &std::path::Path) {
    let mut command = std::process::Command::new("python3");
    command
        .args([
            "-B",
            "-c",
            "import sys; from harbor_db.backup import prune; prune(sys.argv[1],1,2,1024*1024,now=1000000)",
        ])
        .arg(root)
        .env("PYTHONPATH", format!("{}/python", env!("CARGO_MANIFEST_DIR")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let result = harbor_db::storage::process::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
}

#[test]
fn preserves_two_latest_and_recovery_floor() {
    let root = fixture();
    for n in 1..=3 {
        let path = root
            .path()
            .join("wal")
            .join(format!("00000001000000000000000{n}"));
        fs::File::create(&path)
            .unwrap()
            .set_len(1024 * 1024)
            .unwrap();
        fs::File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
    assert!(!root.path().join("base/old").exists());
    assert!(root.path().join("base/middle").exists());
    assert!(!root.path().join("wal/000000010000000000000001").exists());
    assert!(root.path().join("wal/000000010000000000000003").exists());
}

#[test]
fn invalid_expired_manifest_preserves_everything() {
    let root = fixture();
    fs::write(root.path().join("base/old/backup_manifest"), "{}").unwrap();
    backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
    assert!(root.path().join("base/old").exists());
}

fn old_segment(root: &std::path::Path, name: &str, size: u64) -> std::path::PathBuf {
    let path = root.join("wal").join(name);
    let file = fs::File::create(&path).unwrap();
    file.set_len(size).unwrap();
    file.set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
    path
}

#[test]
fn malformed_or_redirected_manifests_fail_conservatively() {
    for bad in [
        json!({}),
        json!({"WAL-Ranges":[]}),
        json!({"WAL-Ranges":{}}),
        json!({"WAL-Ranges":[{"Timeline":true,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}),
        json!({"WAL-Ranges":[{"Timeline":0,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}),
        json!({"WAL-Ranges":[{"Timeline":4294967296u64,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}),
        json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"1/100000000","End-LSN":"2/0"}]}),
        json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/0"}]}),
    ] {
        let root = fixture();
        fs::write(
            root.path().join("base/old/backup_manifest"),
            bad.to_string(),
        )
        .unwrap();
        let wal = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
        backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
        assert!(root.path().join("base/old").exists(), "{bad}");
        assert!(wal.exists());
    }
    for missing in [true, false] {
        let root = fixture();
        let manifest = root.path().join("base/old/backup_manifest");
        fs::remove_file(&manifest).unwrap();
        if !missing {
            std::os::unix::fs::symlink(root.path().join("base/new/backup_manifest"), manifest)
                .unwrap();
        }
        let wal = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
        backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
        assert!(root.path().join("base/old").exists());
        assert!(wal.exists());
    }
}

#[test]
fn wal_size_mismatch_preserves_wal_after_valid_base_expiration() {
    let root = fixture();
    let obsolete = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
    let mismatch = old_segment(root.path(), "000000010000000000000002", 1);
    backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
    assert!(!root.path().join("base/old").exists());
    assert!(obsolete.exists());
    assert!(mismatch.exists());
}

#[test]
fn unknown_timelines_partial_transfers_and_recent_wal_survive() {
    let root = fixture();
    fs::create_dir(root.path().join("base/latest.partial")).unwrap();
    let unknown = old_segment(root.path(), "000000020000000000000001", 1024 * 1024);
    let recent = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
    fs::File::open(&recent)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(999_999)),
        )
        .unwrap();
    let partial = old_segment(root.path(), "000000010000000000000001.partial", 17);
    backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
    assert!(unknown.exists());
    assert!(recent.exists());
    assert!(partial.exists());
    assert!(root.path().join("base/latest.partial").exists());
}

#[test]
fn redirected_or_non_directory_completed_backup_blocks_pruning() {
    for link in [true, false] {
        let root = fixture();
        let path = root.path().join("base/unknown");
        if link {
            std::os::unix::fs::symlink(root.path().join("base/new"), path).unwrap();
        } else {
            fs::write(path, "unknown").unwrap();
        }
        let wal = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
        backup::prune(root.path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
        assert!(root.path().join("base/old").exists());
        assert!(wal.exists());
    }
}

#[test]
fn invalid_segment_size_rejected_before_lock_or_mutation() {
    for size in [
        0,
        1,
        1024 * 1024 - 1,
        3 * 1024 * 1024,
        2 * 1024 * 1024 * 1024,
    ] {
        let root = fixture();
        assert!(backup::prune(root.path(), 1, 2, size, Some(1_000_000.0)).is_err());
        assert!(!root.path().join("BACKUP_LOCK").exists());
    }
}

#[test]
fn python_and_rust_pruning_share_pre_epoch_retention_and_persistent_lock_inode() {
    use std::os::unix::fs::MetadataExt;
    let roots = [fixture(), fixture()];
    for root in &roots {
        for (index, name) in ["old", "middle", "new"].iter().enumerate() {
            fs::File::open(root.path().join("base").join(name))
                .unwrap()
                .set_times(
                    FileTimes::new().set_modified(
                        SystemTime::UNIX_EPOCH - Duration::from_secs(100 - index as u64),
                    ),
                )
                .unwrap();
        }
        for segment in 1..=4 {
            old_segment(
                root.path(),
                &format!("00000001000000000000000{segment}"),
                1024 * 1024,
            );
        }
    }
    python_prune(roots[0].path());
    backup::prune(roots[1].path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
    let inventory = |root: &std::path::Path| {
        let mut paths = std::collections::BTreeMap::new();
        for directory in ["base", "wal"] {
            for entry in fs::read_dir(root.join(directory)).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    paths.insert(
                        format!(
                            "{directory}/{}/backup_manifest",
                            entry.file_name().to_str().unwrap()
                        ),
                        harbor_db::storage::codec::digest(
                            &fs::read(path.join("backup_manifest")).unwrap(),
                        ),
                    );
                } else {
                    paths.insert(
                        format!("{directory}/{}", entry.file_name().to_str().unwrap()),
                        harbor_db::storage::codec::digest(&fs::read(path).unwrap()),
                    );
                }
            }
        }
        paths
    };
    let retained = inventory(roots[0].path());
    assert_eq!(inventory(roots[1].path()), retained);
    assert_eq!(retained.len(), 4);
    assert!(!roots[0].path().join("base/old").exists());
    assert!(roots[0].path().join("base/middle").is_dir());
    assert!(
        !roots[0]
            .path()
            .join("wal/000000010000000000000001")
            .exists()
    );
    assert!(
        roots[0]
            .path()
            .join("wal/000000010000000000000003")
            .is_file()
    );
    let anchors = roots
        .each_ref()
        .map(|root| fs::metadata(root.path().join("BACKUP_LOCK")).unwrap().ino());
    // Continue each implementation's state through its peer, then repeat it.
    for _ in 0..2 {
        backup::prune(roots[0].path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
        python_prune(roots[1].path());
        for (index, root) in roots.iter().enumerate() {
            assert_eq!(inventory(root.path()), retained);
            assert_eq!(
                fs::metadata(root.path().join("BACKUP_LOCK")).unwrap().ino(),
                anchors[index]
            );
        }
    }
}

#[test]
fn python_and_rust_pruning_agree_when_no_eligible_wal_segments_exist() {
    for partial in [false, true] {
        let roots = [fixture(), fixture()];
        for root in &roots {
            let target = root.path().join("empty-wal");
            fs::create_dir(&target).unwrap();
            fs::remove_dir(root.path().join("wal")).unwrap();
            std::os::unix::fs::symlink(&target, root.path().join("wal")).unwrap();
            if partial {
                fs::write(target.join("abandoned.partial"), b"retained diagnostic").unwrap();
            }
        }
        python_prune(roots[0].path());
        backup::prune(roots[1].path(), 1, 2, 1024 * 1024, Some(1_000_000.0)).unwrap();
        for root in &roots {
            assert!(!root.path().join("base/old").exists());
            assert_eq!(fs::read_dir(root.path().join("base")).unwrap().count(), 2);
            assert!(root.path().join("wal").is_symlink());
            assert_eq!(
                fs::read_dir(root.path().join("empty-wal")).unwrap().count(),
                usize::from(partial)
            );
            if partial {
                assert_eq!(
                    fs::read(root.path().join("wal/abandoned.partial")).unwrap(),
                    b"retained diagnostic"
                );
            }
        }
        for name in ["middle", "new"] {
            let relative = format!("base/{name}/backup_manifest");
            assert_eq!(
                fs::read(roots[0].path().join(&relative)).unwrap(),
                fs::read(roots[1].path().join(relative)).unwrap()
            );
        }
    }
}

#[test]
fn malformed_legacy_manifest_and_anchor_inputs_preserve_recovery_bytes() {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    // FIFO manifests retain the approved bounded-admission exception. Invalid
    // JSON and non-file/non-FIFO lock anchors must still preserve recovery data.
    for case in ["fifo-manifest", "directory-lock", "invalid-escape"] {
        let root = fixture();
        let manifest = root.path().join("base/old/backup_manifest");
        let lock = root.path().join("BACKUP_LOCK");
        let special = match case {
            "fifo-manifest" => {
                fs::remove_file(&manifest).unwrap();
                Some(&manifest)
            }
            "directory-lock" => {
                fs::create_dir(&lock).unwrap();
                Some(&lock)
            }
            _ => {
                fs::write(
                    &manifest,
                    br#"{"extra":"\ud80x","WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#,
                )
                .unwrap();
                None
            }
        };
        if let Some(path) = special.filter(|_| case == "fifo-manifest") {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a NUL-terminated fixture path with no open writer.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let wal = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
        let originals: Vec<_> = ["middle", "new"]
            .map(|name| root.path().join(format!("base/{name}/backup_manifest")))
            .into_iter()
            .chain(std::iter::once(wal.clone()))
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                let inode = fs::metadata(&path).unwrap().ino();
                (path, bytes, inode)
            })
            .collect();
        let before = fs::symlink_metadata(special.unwrap_or(&manifest)).unwrap();
        let began = std::time::Instant::now();
        let result = retention_cli(root.path(), true);
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "{case}"
        );
        assert_eq!(
            !result.status.success(),
            case == "directory-lock",
            "{case}: {result:?}"
        );
        assert!(result.stdout.is_empty());
        assert_eq!(fs::read_dir(root.path().join("base")).unwrap().count(), 3);
        assert_eq!(fs::read_dir(root.path().join("wal")).unwrap().count(), 1);
        for (path, bytes, inode) in originals {
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "{case}: {}",
                path.display()
            );
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode, "{case}");
        }
        let after = fs::symlink_metadata(special.unwrap_or(&manifest)).unwrap();
        assert_eq!(
            (after.ino(), after.mode(), after.len()),
            (before.ino(), before.mode(), before.len()),
            "{case}"
        );
        if case == "invalid-escape" {
            assert_eq!(fs::read(&manifest).unwrap(), br#"{"extra":"\ud80x","WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#);
        }
    }
}

fn retention_cli(root: &std::path::Path, native: bool) -> std::process::Output {
    use harbor_db::storage::process::{CommandSpec, run};
    let mut argv = if native {
        vec![env!("CARGO_BIN_EXE_harbor-db-backup-prune").into()]
    } else {
        vec![
            "python3".into(),
            "-B".into(),
            "-m".into(),
            "harbor_db.backup".into(),
        ]
    };
    argv.extend([
        "--root".into(),
        root.to_str().unwrap().into(),
        "--base-days".into(),
        "1".into(),
        "--wal-days".into(),
        "2".into(),
        "--segment-bytes".into(),
        (1024 * 1024).to_string(),
    ]);
    let mut spec = CommandSpec::new(argv);
    let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    environment.insert(
        "PYTHONPATH".into(),
        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
    );
    spec.environment = Some(environment);
    spec.timeout = Duration::from_secs(5);
    run(&spec).unwrap()
}

#[test]
fn python_and_native_cli_fifo_anchor_preserves_inode_and_recovery_through_peer_continuation() {
    use std::os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    };
    for first_native in [false, true] {
        let root = fixture();
        let anchor = root.path().join("BACKUP_LOCK");
        let name = std::ffi::CString::new(anchor.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated fixture path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let obsolete = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
        let floor = old_segment(root.path(), "000000010000000000000003", 1024 * 1024);
        let unknown = old_segment(root.path(), "000000020000000000000001", 1024 * 1024);
        fs::write(
            root.path().join("base/middle/recovery-data"),
            b"required middle bytes",
        )
        .unwrap();
        fs::write(
            root.path().join("base/new/recovery-data"),
            b"required latest bytes",
        )
        .unwrap();
        let snapshot = |path: &std::path::Path| {
            let metadata = fs::symlink_metadata(path).unwrap();
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.len(),
            )
        };
        let anchor_before = snapshot(&anchor);
        let retained: Vec<_> = [
            root.path().join("base/middle/backup_manifest"),
            root.path().join("base/new/backup_manifest"),
            root.path().join("base/middle/recovery-data"),
            root.path().join("base/new/recovery-data"),
            floor,
            unknown,
        ]
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            let metadata = snapshot(&path);
            (path, bytes, metadata)
        })
        .collect();
        assert!(harbor_db::storage::durable::lock(&anchor, false, false).is_err());
        let owner = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&anchor)
            .unwrap();
        // SAFETY: owner holds the opened FIFO descriptor through this flock.
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        for native in [false, true] {
            let began = std::time::Instant::now();
            let output = retention_cli(root.path(), native);
            assert!(!output.status.success(), "contended FIFO: native={native}");
            assert!(began.elapsed() < Duration::from_secs(2));
            assert!(obsolete.exists());
            assert!(root.path().join("base/old").is_dir());
            assert_eq!(snapshot(&anchor), anchor_before);
        }
        drop(owner);
        for native in [first_native, !first_native, first_native, !first_native] {
            let output = retention_cli(root.path(), native);
            assert!(
                output.status.success(),
                "native={native}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
            assert!(!obsolete.exists());
            assert!(!root.path().join("base/old").exists());
            assert_eq!(snapshot(&anchor), anchor_before);
            for (path, bytes, metadata) in &retained {
                assert_eq!(&fs::read(path).unwrap(), bytes);
                assert_eq!(&snapshot(path), metadata);
            }
        }
    }
}

#[test]
fn python_and_native_cli_unused_surrogates_and_last_decoded_keys_accept_retention() {
    use std::os::unix::fs::MetadataExt;
    for manifest in [
        br#"{"extra":"\ud800","\udfff":"\udc00","WAL-Ranges":[{"unused":"\udfff","\ud800":{},"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#.as_slice(),
        br#"{"WAL-Ranges":"\ud800","WAL-\u0052anges":[{"Timeline":"\udfff","Time\u006cine":1,"Start-LSN":"\ud800","Start-\u004cSN":"0/300000","End-LSN":false,"End-\u004cSN":"0/400000","extra":["\ud800",{"\udc00":"\udfff"}]}]}"#.as_slice(),
        br#"{"extra":"\\\"}[","WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#.as_slice(),
        br#"{"extra":{"$serde_json::private::RawValue":"not JSON"},"WAL-Ranges":[{"Timeline":{"$serde_json::private::RawValue":"1"},"Timeline":1,"Start-LSN":{"$serde_json::private::RawValue":"\"0/300000\""},"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#.as_slice(),
    ] {
        for first_native in [false, true] {
            let root = fixture();
            for name in ["old", "middle", "new"] {
                fs::write(root.path().join(format!("base/{name}/backup_manifest")), manifest).unwrap();
            }
            let obsolete = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
            let floor = old_segment(root.path(), "000000010000000000000003", 1024 * 1024);
            let above = old_segment(root.path(), "000000010000000000000004", 1024 * 1024);
            let unknown = old_segment(root.path(), "000000020000000000000001", 1024 * 1024);
            let originals: Vec<_> = [
                root.path().join("base/middle/backup_manifest"),
                root.path().join("base/new/backup_manifest"), floor, above, unknown,
            ].into_iter().map(|path| {
                let bytes = fs::read(&path).unwrap();
                let m = fs::metadata(&path).unwrap();
                (path, bytes, m.dev(), m.ino(), m.mode())
            }).collect();
            let mut anchor_inode = None;
            for native in [first_native, !first_native, first_native, !first_native] {
                let output = retention_cli(root.path(), native);
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                assert!(output.stdout.is_empty());
                assert!(output.stderr.is_empty());
                assert!(!root.path().join("base/old").exists(), "native={native}, manifest={manifest:?}");
                assert!(!obsolete.exists());
                let inode = fs::metadata(root.path().join("BACKUP_LOCK")).unwrap().ino();
                assert_eq!(*anchor_inode.get_or_insert(inode), inode);
                for (path, bytes, dev, inode, mode) in &originals {
                    assert_eq!(&fs::read(path).unwrap(), bytes);
                    let m = fs::metadata(path).unwrap();
                    assert_eq!((m.dev(), m.ino(), m.mode()), (*dev, *inode, *mode));
                }
            }
        }
    }
}

#[test]
fn retention_projection_rejects_invalid_syntax_and_last_fields_before_deletion() {
    use std::os::unix::fs::MetadataExt;
    let valid = r#"[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]"#;
    let mut cases = vec![
        format!(r#"{{"WAL-Ranges":{valid},"unused":"\x20"}}"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"unused":"\ud80x"}}"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"unused":[true,]}}"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"unused":{{"key":}}}}"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"unused":1.}}"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid}}} false"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"unused":"truncated"#).into_bytes(),
        format!(r#"{{"WAL-Ranges":{valid},"WAL-\u0052anges":"\udfff"}}"#).into_bytes(),
        br#"{"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","Start-\u004cSN":"\ud800","End-LSN":"0/400000"}]}"#.to_vec(),
        br#"{"WAL-Ranges":[{"Timeline":1,"Timeline":true,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#.to_vec(),
        br#"{"WAL-Ranges":[{"Timeline":{"$serde_json::private::RawValue":"1"},"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#.to_vec(),
        br#"{"WAL-Ranges":[{"Timeline":1,"Start-LSN":{"$serde_json::private::RawValue":"\"0/300000\""},"End-LSN":"0/400000"}]}"#.to_vec(),
        br#"{"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":{"$serde_json::private::RawValue":"\"0/400000\""}}]}"#.to_vec(),
    ];
    let mut invalid_key = format!(r#"{{"WAL-Ranges":{valid},"unused"#).into_bytes();
    invalid_key.extend_from_slice(b"\xff\":null}");
    cases.push(invalid_key);
    for bytes in cases {
        for native in [false, true] {
            let root = fixture();
            let manifest = root.path().join("base/old/backup_manifest");
            fs::write(&manifest, &bytes).unwrap();
            drop(
                harbor_db::storage::durable::lock(&root.path().join("BACKUP_LOCK"), false, true)
                    .unwrap(),
            );
            let obsolete = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
            let required = old_segment(root.path(), "000000010000000000000003", 1024 * 1024);
            let paths = [
                manifest,
                root.path().join("base/middle/backup_manifest"),
                root.path().join("base/new/backup_manifest"),
                obsolete,
                required,
                root.path().join("BACKUP_LOCK"),
            ];
            let snapshot = || {
                paths
                    .iter()
                    .map(|path| {
                        let m = fs::symlink_metadata(path).unwrap();
                        (
                            fs::read(path).unwrap(),
                            m.dev(),
                            m.ino(),
                            m.mode(),
                            m.uid(),
                            m.len(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let before = snapshot();
            let output = retention_cli(root.path(), native);
            assert!(output.status.success(), "native={native}, bytes={bytes:?}");
            assert!(output.stdout.is_empty() && output.stderr.is_empty());
            assert!(
                paths.iter().all(|path| path.exists()),
                "deleted recovery artifact: native={native}, bytes={bytes:?}"
            );
            assert_eq!(snapshot(), before, "native={native}, bytes={bytes:?}");
            assert_eq!(fs::read_dir(root.path().join("base")).unwrap().count(), 3);
        }
    }
    // Keep the existing native JSON nesting bound even for unused raw values.
    let root = fixture();
    let excessive = format!(
        r#"{{"unused":{}0{},"WAL-Ranges":{valid}}}"#,
        "[".repeat(128),
        "]".repeat(128)
    );
    let manifest = root.path().join("base/old/backup_manifest");
    fs::write(&manifest, excessive.as_bytes()).unwrap();
    let obsolete = old_segment(root.path(), "000000010000000000000001", 1024 * 1024);
    let inode = fs::metadata(&manifest).unwrap().ino();
    let output = retention_cli(root.path(), true);
    assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    assert!(obsolete.exists() && root.path().join("base/old").is_dir());
    assert_eq!(fs::read(&manifest).unwrap(), excessive.as_bytes());
    assert_eq!(fs::metadata(&manifest).unwrap().ino(), inode);
}
