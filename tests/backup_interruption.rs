#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use harbor_db::storage::{StorageError, codec, durable, process};
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    io::Read,
    os::unix::{ffi::OsStringExt, fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime},
};

const LIMIT: Duration = Duration::from_secs(10);

struct Worker {
    child: Child,
    reaped: bool,
}
impl Worker {
    fn event(&mut self, deadline: Instant) -> i32 {
        loop {
            let mut status = 0;
            // SAFETY: wait only for our owned child, into live status storage.
            let result = unsafe {
                libc::waitpid(
                    self.child.id() as i32,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED,
                )
            };
            if result > 0 {
                self.reaped = libc::WIFEXITED(status) || libc::WIFSIGNALED(status);
                return status;
            }
            assert_eq!(result, 0, "waitpid: {}", std::io::Error::last_os_error());
            assert!(Instant::now() < deadline, "native pruner event timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                return status;
            }
            assert!(Instant::now() < deadline, "native pruner exit timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let deadline = Instant::now() + LIMIT;
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        }
    }
}

fn trace(pid: u32, request: libc::c_uint, data: usize) {
    // SAFETY: commands target only our stopped child. GETREGS receives live,
    // correctly sized user_regs_struct storage; other commands use scalar data.
    let result = unsafe { libc::ptrace(request, pid as i32, 0usize, data) };
    assert_ne!(
        result,
        -1,
        "ptrace {request}: {}",
        std::io::Error::last_os_error()
    );
}

fn filename(pid: u32, address: u64) -> PathBuf {
    let mut bytes = Vec::new();
    for offset in (0..4096).step_by(size_of::<libc::c_long>()) {
        // SAFETY: errno is thread-local; PEEKDATA copies from our stopped child,
        // not a pointer dereference in this process. -1 can be a valid word.
        let word = unsafe {
            *libc::__errno_location() = 0;
            let word = libc::ptrace(
                libc::PTRACE_PEEKDATA,
                pid as i32,
                address + offset as u64,
                0usize,
            );
            assert_eq!(*libc::__errno_location(), 0, "PEEKDATA failed");
            word
        };
        for byte in word.to_ne_bytes() {
            if byte == 0 {
                return PathBuf::from(std::ffi::OsString::from_vec(bytes));
            }
            bytes.push(byte);
        }
    }
    panic!("unlink pathname exceeds 4096 bytes");
}

// Include directory entries and immutable identity metadata, but exclude atime:
// our own content verification legitimately reads these files.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    identity: (u64, u64, u32, u32, u32, u64, i64, i64),
    digest: Option<String>,
}
fn inventory(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        let m = fs::symlink_metadata(path).unwrap();
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            Entry {
                identity: (
                    m.dev(),
                    m.ino(),
                    m.mode(),
                    m.uid(),
                    m.gid(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                ),
                digest: m.is_file().then(|| codec::digest(&fs::read(path).unwrap())),
            },
        );
        if m.is_dir() {
            for child in fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    for name in ["base", "wal"] {
        visit(root, &root.join(name), &mut entries);
    }
    entries
}
fn command(root: &Path, log: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_harbor-db-backup-prune"));
    cmd.arg("--root")
        .arg(root)
        .args([
            "--base-days",
            "1",
            "--wal-days",
            "2",
            "--segment-bytes",
            "1048576",
        ])
        .stdin(Stdio::null())
        .stdout(File::create(log.with_extension("stdout")).unwrap())
        .stderr(File::create(log.with_extension("stderr")).unwrap());
    cmd
}
fn logs(log: &Path) -> String {
    let mut text = String::new();
    for extension in ["stdout", "stderr"] {
        File::open(log.with_extension(extension))
            .unwrap()
            .take(8192)
            .read_to_string(&mut text)
            .unwrap();
    }
    text
}

#[test]
fn interrupted_cli_prune_preserves_chain_and_retries_under_same_anchor() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("backup");
    for name in [
        "base/old",
        "base/middle",
        "base/new",
        "base/upload.partial",
        "wal",
    ] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    for (index, name) in ["old", "middle", "new"].iter().enumerate() {
        fs::write(
            root.join(format!("base/{name}/backup_manifest")),
            br#"{"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/300000","End-LSN":"0/400000"}]}"#,
        )
        .unwrap();
        fs::write(
            root.join(format!("base/{name}/recovery-data")),
            format!("nonempty recovery payload: {name}"),
        )
        .unwrap();
        File::open(root.join(format!("base/{name}")))
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100 + index as u64)),
            )
            .unwrap();
    }
    fs::write(
        root.join("base/upload.partial/payload"),
        b"unfinished base transfer",
    )
    .unwrap();
    fs::write(
        root.join("wal/transfer.partial"),
        b"unfinished WAL transfer",
    )
    .unwrap();
    for segment in 1..=4 {
        let path = root.join(format!("wal/00000001000000000000000{segment}"));
        fs::write(&path, vec![segment as u8; 1048576]).unwrap();
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    let before = inventory(&root);
    let log = temp.path().join("interrupted");
    let mut cmd = command(&root, &log);
    // SAFETY: child-side pre_exec performs only the async-signal-safe ptrace
    // syscall and errno capture. No SIGSTOP: exec must close Rust's error pipe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0usize, 0usize) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut worker = Worker {
        child: process::spawn(&mut cmd).unwrap(),
        reaped: false,
    };
    let pid = worker.child.id();
    let deadline = Instant::now() + LIMIT;
    let status = worker.event(deadline);
    assert!(
        libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGTRAP,
        "exec stop: {status}, {}",
        logs(&log)
    );
    trace(
        pid,
        libc::PTRACE_SETOPTIONS,
        (libc::PTRACE_O_TRACESYSGOOD | libc::PTRACE_O_EXITKILL) as usize,
    );
    // Linux ptrace(2): SYSCALL stops alternate entry/exit after this exec stop.
    // Reject unexpected signal/event stops rather than silently losing alignment.
    // Source: https://man7.org/linux/man-pages/man2/ptrace.2.html
    let mut entry = true;
    loop {
        trace(pid, libc::PTRACE_SYSCALL, 0);
        let status = worker.event(deadline);
        assert!(
            libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGTRAP | 0x80,
            "syscall stop: {status}, {}",
            logs(&log)
        );
        if entry {
            // SAFETY: all-zero integers are valid register storage, filled below.
            let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            trace(
                pid,
                libc::PTRACE_GETREGS,
                (&mut regs as *mut libc::user_regs_struct) as usize,
            );
            let target = if regs.orig_rax == libc::SYS_unlinkat as u64 {
                let path = filename(pid, regs.rsi);
                let directory = if regs.rdi as i32 == libc::AT_FDCWD {
                    fs::read_link(format!("/proc/{pid}/cwd")).unwrap()
                } else {
                    fs::read_link(format!("/proc/{pid}/fd/{}", regs.rdi as i32)).unwrap()
                };
                Some(directory.join(path))
            } else if regs.orig_rax == libc::SYS_unlink as u64 {
                Some(
                    fs::read_link(format!("/proc/{pid}/cwd"))
                        .unwrap()
                        .join(filename(pid, regs.rdi)),
                )
            } else {
                None
            };
            if let Some(target) = target.filter(|path| path.starts_with(root.join("base/old"))) {
                assert!(
                    target.is_file(),
                    "actual first deletion target: {}",
                    target.display()
                );
                break;
            }
        }
        entry = !entry;
    }
    assert!(worker.child.try_wait().unwrap().is_none());
    let anchor = fs::metadata(root.join("BACKUP_LOCK")).unwrap();
    let anchor_id = (anchor.dev(), anchor.ino());
    assert!(
        matches!(durable::lock(&root.join("BACKUP_LOCK"), false, true), Err(StorageError::Io(error)) if error.raw_os_error() == Some(libc::EAGAIN))
    );
    assert_eq!(
        inventory(&root),
        before,
        "before-delete stop changed recovery inventory"
    );
    worker.child.kill().unwrap();
    let killed = worker.event(Instant::now() + LIMIT);
    assert!(
        libc::WIFSIGNALED(killed) && libc::WTERMSIG(killed) == libc::SIGKILL,
        "kill status: {killed}, {}",
        logs(&log)
    );
    let authority = durable::lock(&root.join("BACKUP_LOCK"), false, true).unwrap();
    let anchor = fs::metadata(root.join("BACKUP_LOCK")).unwrap();
    assert_eq!((anchor.dev(), anchor.ino()), anchor_id);
    assert_eq!(
        inventory(&root),
        before,
        "SIGKILL changed recovery inventory"
    );
    drop(authority);

    let log = temp.path().join("retry");
    let mut retry = Worker {
        child: process::spawn(&mut command(&root, &log)).unwrap(),
        reaped: false,
    };
    assert!(
        retry.finish().success(),
        "explicit native retry: {}",
        logs(&log)
    );
    let after = inventory(&root);
    let expected: BTreeMap<_, _> = before
        .into_iter()
        .filter(|(path, _)| {
            !path.starts_with("base/old")
                && ![
                    "wal/000000010000000000000001",
                    "wal/000000010000000000000002",
                ]
                .iter()
                .any(|name| path == Path::new(name))
        })
        .collect();
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (path, entry) in expected {
        if path != Path::new("base") && path != Path::new("wal") {
            assert_eq!(
                after[&path],
                entry,
                "retained recovery entry: {}",
                path.display()
            );
        }
    }
    assert!(
        !fs::read(root.join("base/new/recovery-data"))
            .unwrap()
            .is_empty()
    );
    assert!(
        !fs::read(root.join("base/new/backup_manifest"))
            .unwrap()
            .is_empty()
    );
    let anchor = fs::metadata(root.join("BACKUP_LOCK")).unwrap();
    assert_eq!((anchor.dev(), anchor.ino()), anchor_id);
    assert!(durable::lock(&root.join("BACKUP_LOCK"), false, true).is_ok());
}
