use harbor_db::storage::process::{self, CommandSpec};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn python() -> String {
    let path = std::env::var_os("HARBOR_DB_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH").expect("approved PATH"))
                .map(|directory| directory.join("python3"))
                .find(|path| path.is_file())
                .expect("approved environment provides Python")
        });
    assert!(
        path.is_absolute(),
        "Python must be resolved to an absolute path"
    );
    path.to_str().unwrap().into()
}

fn payload(seed: u8) -> Vec<u8> {
    (0..3 * 1024 * 1024)
        .map(|index| (index as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn publish(native: bool, tree: bool, source: &Path, destination: &Path) -> Output {
    assert!(source.is_absolute() && destination.is_absolute());
    let mut argv = if native {
        vec![env!("CARGO_BIN_EXE_harbor-db-durable").into()]
    } else {
        vec![
            python(),
            "-B".into(),
            "-m".into(),
            "harbor_db.durable".into(),
        ]
    };
    argv.extend([
        if tree { "publish-tree" } else { "publish-file" }.into(),
        source.to_str().unwrap().into(),
        destination.to_str().unwrap().into(),
    ]);
    let mut spec = CommandSpec::new(argv);
    spec.timeout = Duration::from_secs(15);
    spec.environment = Some(BTreeMap::from([(
        "PYTHONPATH".into(),
        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
    )]));
    Worker::capture(&spec).finish()
}

fn accepted(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

fn occupied(output: &Output, native: bool) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error = std::str::from_utf8(&output.stderr).unwrap();
    if native {
        assert!(error.starts_with("harbor-db-durable: "), "{error}");
        assert!(error.contains("(os error 17)"), "{error}");
    } else {
        assert_eq!(
            error,
            "harbor-db-durable: publication destination already exists\n"
        );
    }
}

// Only a launch barrier: publication is executed by the unmodified public CLI.
const BARRIER: &str = "import os,sys\nprint('ready',flush=True)\nassert sys.stdin.buffer.read(1)==b'G'\nos.execv(sys.argv[1],sys.argv[1:])\n";

struct Worker {
    child: Child,
    stdout: Option<thread::JoinHandle<Vec<u8>>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
    ready: mpsc::Receiver<String>,
}

impl Worker {
    // CommandSpec retains child-local environment intent; run() suppresses
    // diagnostics, so use the central spawn gate and collect both public streams.
    fn capture(spec: &CommandSpec) -> Self {
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(environment) = &spec.environment {
            command.env_clear().envs(environment);
        }
        let mut child = process::spawn(&mut command).unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (_, ready) = mpsc::channel();
        Self {
            child,
            stdout: Some(thread::spawn(move || {
                let mut bytes = vec![];
                stdout.read_to_end(&mut bytes).unwrap();
                bytes
            })),
            stderr: Some(thread::spawn(move || {
                let mut bytes = vec![];
                stderr.read_to_end(&mut bytes).unwrap();
                bytes
            })),
            ready,
        }
    }

    fn start(source: &Path, destination: &Path) -> Self {
        let mut command = Command::new(python());
        command
            .args([
                "-B",
                "-c",
                BARRIER,
                env!("CARGO_BIN_EXE_harbor-db-durable"),
                "publish-file",
            ])
            .arg(source)
            .arg(destination)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process::spawn(&mut command).unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (sender, ready) = mpsc::channel();
        Self {
            child,
            stdout: Some(thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                sender.send(line).unwrap();
                let mut bytes = vec![];
                reader.read_to_end(&mut bytes).unwrap();
                bytes
            })),
            stderr: Some(thread::spawn(move || {
                let mut bytes = vec![];
                stderr.read_to_end(&mut bytes).unwrap();
                bytes
            })),
            ready,
        }
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "publisher deadline exceeded");
            thread::sleep(Duration::from_millis(2));
        };
        Output {
            status,
            stdout: self.stdout.take().unwrap().join().unwrap(),
            stderr: self.stderr.take().unwrap().join().unwrap(),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn concurrent_native_file_publishers_acknowledge_one_complete_winner_and_refuse_repeat() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("published.bin");
    let sources = [
        temp.path().join("one.partial"),
        temp.path().join("two.partial"),
    ];
    let bytes = [payload(19), payload(203)];
    for index in 0..2 {
        fs::write(&sources[index], &bytes[index]).unwrap();
    }
    let identities = sources.each_ref().map(|source| identity(source));
    let mut workers = sources
        .each_ref()
        .map(|source| Worker::start(source, &destination));
    for worker in &workers {
        assert_eq!(
            worker.ready.recv_timeout(Duration::from_secs(15)).unwrap(),
            "ready\n"
        );
    }
    assert!(!destination.exists());
    for worker in &mut workers {
        worker.child.stdin.take().unwrap().write_all(b"G").unwrap();
    }
    let outputs = workers.each_mut().map(Worker::finish);
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1
    );
    let winner = usize::from(!outputs[0].status.success());
    let loser = 1 - winner;
    accepted(&outputs[winner]);
    occupied(&outputs[loser], true);
    assert!(
        !sources[winner].exists(),
        "acknowledged native publication consumes source"
    );
    assert_eq!(identity(&destination), identities[winner]);
    assert_eq!(fs::read(&destination).unwrap(), bytes[winner]);
    assert_eq!(identity(&sources[loser]), identities[loser]);
    assert_eq!(fs::read(&sources[loser]).unwrap(), bytes[loser]);
    let before = snapshot(&destination);
    occupied(&publish(true, false, &sources[loser], &destination), true);
    assert_eq!(snapshot(&destination), before);
    assert_eq!(identity(&sources[loser]), identities[loser]);
    assert_eq!(fs::read(&sources[loser]).unwrap(), bytes[loser]);
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    bytes: Option<Vec<u8>>,
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.is_file() || metadata.is_dir());
        entries.insert(
            path.strip_prefix(root).unwrap().into(),
            Entry {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                length: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                bytes: metadata.is_file().then(|| fs::read(path).unwrap()),
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn receipt(bytes: &[u8], producer: &str) -> Vec<u8> {
    let mut result = serde_json::to_vec(&json!({
        "producer": producer, "sha256": format!("{:x}", Sha256::digest(bytes)), "version": 1,
    }))
    .unwrap();
    result.push(b'\n');
    result
}

fn verify_receipt(path: &Path, expected: &[u8], payload: &[u8]) {
    let actual = fs::read(path).unwrap();
    assert_eq!(actual, expected);
    let value: serde_json::Value = serde_json::from_slice(&actual).unwrap();
    assert_eq!(value["sha256"], format!("{:x}", Sha256::digest(payload)));
    let mut canonical = serde_json::to_vec(&value).unwrap();
    canonical.push(b'\n');
    assert_eq!(actual, canonical);
}

#[test]
fn python_and_native_preserve_published_files_and_trees_then_retry_retained_intent() {
    for native_first in [false, true] {
        for tree in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let private = temp.path().join("private");
            let public = temp.path().join("public");
            fs::create_dir(&private).unwrap();
            fs::create_dir(&public).unwrap();
            let original = private.join("original.partial");
            let pending = private.join("pending.partial");
            let destination = public.join("published");
            let retry = public.join("explicit-retry");
            let original_bytes = payload(7);
            let pending_bytes = payload(129);
            let original_receipt = receipt(
                &original_bytes,
                if native_first { "native" } else { "python" },
            );
            let pending_receipt = receipt(
                &pending_bytes,
                if native_first { "python" } else { "native" },
            );
            for (source, bytes, receipt) in [
                (&original, &original_bytes, &original_receipt),
                (&pending, &pending_bytes, &pending_receipt),
            ] {
                if tree {
                    fs::create_dir(source).unwrap();
                    fs::create_dir(source.join("nested")).unwrap();
                    fs::write(source.join("nested/records.bin"), bytes).unwrap();
                    fs::write(source.join("receipt.json"), receipt).unwrap();
                } else {
                    fs::write(source, bytes).unwrap();
                }
            }
            let original_identity = identity(&original);
            accepted(&publish(native_first, tree, &original, &destination));
            assert!(!original.exists());
            assert_eq!(identity(&destination), original_identity);
            let original_record = if tree {
                destination.join("nested/records.bin")
            } else {
                destination.clone()
            };
            let original_receipt_path = if tree {
                destination.join("receipt.json")
            } else {
                public.join("original-receipt.json")
            };
            if !tree {
                let receipt_source = private.join("original-receipt.partial");
                fs::write(&receipt_source, &original_receipt).unwrap();
                accepted(&publish(
                    native_first,
                    false,
                    &receipt_source,
                    &original_receipt_path,
                ));
                assert!(!receipt_source.exists());
            }
            let acknowledged_bytes = fs::read(&original_record).unwrap();
            assert_eq!(acknowledged_bytes, original_bytes);
            verify_receipt(
                &original_receipt_path,
                &original_receipt,
                &acknowledged_bytes,
            );
            let published_before = snapshot(&destination);
            let pending_before = snapshot(&pending);
            occupied(
                &publish(!native_first, tree, &pending, &destination),
                !native_first,
            );
            assert_eq!(snapshot(&destination), published_before);
            assert_eq!(snapshot(&pending), pending_before);
            let pending_identity = identity(&pending);
            accepted(&publish(!native_first, tree, &pending, &retry));
            assert!(
                !pending.exists(),
                "retry consumes the original refused source"
            );
            assert_eq!(identity(&retry), pending_identity);
            assert_eq!(snapshot(&destination), published_before);
            let retry_record = if tree {
                retry.join("nested/records.bin")
            } else {
                retry.clone()
            };
            let retry_receipt_path = if tree {
                retry.join("receipt.json")
            } else {
                public.join("retry-receipt.json")
            };
            if !tree {
                let receipt_source = private.join("retry-receipt.partial");
                fs::write(&receipt_source, &pending_receipt).unwrap();
                accepted(&publish(
                    !native_first,
                    false,
                    &receipt_source,
                    &retry_receipt_path,
                ));
                assert!(!receipt_source.exists());
            }
            let retried_bytes = fs::read(&retry_record).unwrap();
            assert_eq!(retried_bytes, pending_bytes);
            verify_receipt(&retry_receipt_path, &pending_receipt, &retried_bytes);
            verify_receipt(
                &original_receipt_path,
                &original_receipt,
                &fs::read(&original_record).unwrap(),
            );
        }
    }
}
