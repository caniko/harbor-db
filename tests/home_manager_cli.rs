#![cfg(unix)]

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

fn invoke(root: &Path, args: &[&OsStr], extension: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_home-manager-backup"));
    command
        .current_dir(root)
        .args(args)
        .env_remove("HOME_MANAGER_BACKUP_EXT");
    if let Some(extension) = extension {
        command.env("HOME_MANAGER_BACKUP_EXT", extension);
    }
    command
        .output()
        .expect("execute actual Home Manager backup binary")
}

fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    metadata: [u64; 9],
    bytes: Vec<u8>,
}

fn snapshot(root: &Path) -> Vec<Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<Entry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let bytes = if metadata.is_file() {
            fs::read(path).unwrap()
        } else {
            Vec::new()
        };
        entries.push(Entry {
            path: path.strip_prefix(root).unwrap().to_owned(),
            // Reading the snapshot can update atime; retain identity, modes,
            // ownership, size and mutation times rather than access times.
            metadata: [
                metadata.ino(),
                metadata.mode().into(),
                metadata.uid().into(),
                metadata.gid().into(),
                metadata.len(),
                metadata.mtime() as u64,
                metadata.mtime_nsec() as u64,
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
            ],
            bytes,
        });
        if metadata.is_dir() {
            for child in fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn assert_failure(output: &Output, code: i32, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with(&format!("home-manager-backup: {diagnostic}")),
        "{output:?}"
    );
}

fn archives(root: &Path, backup: &Path) -> Vec<PathBuf> {
    let mut prefix = backup.file_name().unwrap().to_os_string().into_vec();
    prefix.extend_from_slice(b".canix-");
    let mut found = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_vec();
        if let Some(stamp) = name.strip_prefix(prefix.as_slice()) {
            let stamp = std::str::from_utf8(stamp).expect("ASCII timestamp suffix");
            assert!(stamp.len() >= 16, "{stamp}");
            let (timestamp, collision) = stamp.split_at(16);
            assert!(timestamp.as_bytes()[..8].iter().all(u8::is_ascii_digit));
            assert_eq!(&timestamp[8..9], "T");
            assert!(timestamp.as_bytes()[9..15].iter().all(u8::is_ascii_digit));
            assert_eq!(&timestamp[15..], "Z");
            chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ")
                .expect("real calendar timestamp in public archive format");
            if !collision.is_empty() {
                assert!(collision.starts_with('-'));
                let index = collision[1..]
                    .parse::<u64>()
                    .expect("numeric collision suffix");
                assert!(index > 0);
                assert_eq!(collision, format!("-{index}"));
            }
            found.push(entry.path());
        }
    }
    found.sort();
    found
}

#[test]
fn literal_path_and_environment_preserve_three_revisions_across_explicit_retry() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    let target = root.join(OsString::from_vec(
        b"settings with spaces ; $literal-\xff".to_vec(),
    ));
    let extension = "archived suffix";
    let backup = suffix(&target, &format!(".{extension}"));
    let old = b"old\0\xff\r\n";
    let current = b"current\0\xfe\n";
    let third = b"third\0\xfd\r\n";
    fs::write(&target, current).unwrap();
    fs::write(&backup, old).unwrap();

    let output = invoke(root, &[target.as_os_str()], Some(extension));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!target.exists());
    assert_eq!(fs::read(&backup).unwrap(), current);
    let first_archives = archives(root, &backup);
    assert_eq!(first_archives.len(), 1);
    assert_eq!(fs::read(&first_archives[0]).unwrap(), old);
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);

    let before = snapshot(root);
    let output = invoke(root, &[target.as_os_str()], Some(extension));
    assert_failure(&output, 1, "Home Manager collision does not exist:");
    assert_eq!(snapshot(root), before);

    fs::write(&target, third).unwrap();
    let output = invoke(root, &[target.as_os_str()], Some(extension));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!target.exists());
    assert_eq!(fs::read(&backup).unwrap(), third);
    assert_eq!(fs::read(&first_archives[0]).unwrap(), old);
    let retained = archives(root, &backup);
    assert_eq!(retained.len(), 2);
    let second_archive = retained
        .iter()
        .find(|path| **path != first_archives[0])
        .unwrap();
    assert_eq!(fs::read(second_archive).unwrap(), current);
    assert_eq!(fs::read_dir(root).unwrap().count(), 3);
}

#[test]
fn parser_and_environment_failures_preserve_entire_tree_and_public_exit_codes() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    let target = root.join("settings with spaces");
    fs::write(&target, b"current\0\xff").unwrap();
    fs::write(suffix(&target, ".archived suffix"), b"previous\0\xfe").unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/unrelated"), b"unrelated\0\xfd").unwrap();
    let absent = root.join("absent");
    let before = snapshot(root);
    let cases: Vec<(Vec<&OsStr>, Option<&str>, i32, &str)> = vec![
        (
            vec![],
            Some("archived suffix"),
            2,
            "usage: home-manager-backup <target>",
        ),
        (
            vec![target.as_os_str(), OsStr::new("extra")],
            Some("archived suffix"),
            2,
            "usage: home-manager-backup <target>",
        ),
        (
            vec![target.as_os_str()],
            None,
            1,
            "HOME_MANAGER_BACKUP_EXT is not set by Home Manager",
        ),
        (
            vec![target.as_os_str()],
            Some(""),
            1,
            "HOME_MANAGER_BACKUP_EXT must be a non-empty file-name suffix",
        ),
        (
            vec![target.as_os_str()],
            Some("invalid/suffix"),
            1,
            "HOME_MANAGER_BACKUP_EXT must be a non-empty file-name suffix",
        ),
        (
            vec![absent.as_os_str()],
            Some("archived suffix"),
            1,
            "Home Manager collision does not exist:",
        ),
    ];
    for (args, extension, code, diagnostic) in cases {
        let output = invoke(root, &args, extension);
        assert_failure(&output, code, diagnostic);
        assert_eq!(
            snapshot(root),
            before,
            "mutation after {args:?}, {extension:?}"
        );
    }
}
