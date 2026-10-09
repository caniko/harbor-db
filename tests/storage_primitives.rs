use harbor_db::storage::{codec, durable, process};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::symlink,
    time::{Duration, Instant},
};

#[test]
fn failed_writer_exec_restores_flags_before_unrelated_spawn() {
    const CHILD: &str = "HARBOR_DB_EXEC_HANDOVER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut spec = process::CommandSpec::new(vec![
            std::env::current_exe().unwrap().to_str().unwrap().into(),
            "--exact".into(),
            "failed_writer_exec_restores_flags_before_unrelated_spawn".into(),
            "--nocapture".into(),
        ]);
        let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
        environment.insert(CHILD.into(), "1".into());
        spec.environment = Some(environment);
        spec.timeout = Duration::from_secs(20);
        let output = process::run(&spec).unwrap();
        assert!(
            output.status.success(),
            "owned handover subprocess failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use std::{
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        },
    };
    fn flags(fd: i32) -> i32 {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        flags
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("authority.lock");
    fs::write(&path, b"").unwrap();
    let lease = durable::lock(&path, false, false).unwrap();
    let second_path = root.path().join("second.lock");
    fs::write(&second_path, b"").unwrap();
    let second = durable::lock(&second_path, false, false).unwrap();
    let ordinary = fs::File::open("/dev/null").unwrap();
    assert_eq!(
        unsafe { libc::fcntl(ordinary.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let selected = [lease.fd(), second.fd(), ordinary.as_raw_fd()];
    let originals = selected.map(flags);
    assert_ne!(originals[0] & libc::FD_CLOEXEC, 0);
    assert_ne!(originals[1] & libc::FD_CLOEXEC, 0);
    assert_eq!(originals[2], 0);
    assert!(
        matches!(durable::lock(&path, true, false), Err(harbor_db::storage::StorageError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );

    // An invalid later descriptor must not mutate an earlier valid descriptor.
    let mut invalid = std::process::Command::new(root.path().join("missing"));
    assert!(
        matches!(process::exec(&mut invalid, &[lease.fd(), -1]), Err(harbor_db::storage::StorageError::Io(error)) if error.raw_os_error() == Some(libc::EBADF))
    );
    assert_eq!(selected.map(flags), originals);
    assert!(
        matches!(process::exec(&mut invalid, &selected), Err(harbor_db::storage::StorageError::Io(error)) if error.raw_os_error() == Some(libc::ENOENT))
    );
    assert_eq!(selected.map(flags), originals);

    let (hook, mut worker_pipe) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let mut byte = [0];
        worker_pipe.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        // Acknowledge before trying spawn: the exec hook holds the gate, so it
        // must not wait for child execution or for spawn to complete.
        worker_pipe.write_all(&[2]).unwrap();
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        process::spawn(&mut command).unwrap()
    });
    let mut command = std::process::Command::new("true");
    let hook_fd = hook.as_raw_fd();
    // CommandExt::exec runs this hook in the owned subprocess itself, not in a
    // post-fork copy of a multithreaded test harness. Only syscalls and stack
    // storage are used while the unrelated spawn thread requests handover.
    unsafe {
        command.pre_exec(move || {
            for fd in selected {
                let current = libc::fcntl(fd, libc::F_GETFD);
                if current < 0 || current & libc::FD_CLOEXEC != 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
            }
            let mut byte = 1u8;
            if libc::write(hook_fd, (&byte as *const u8).cast(), 1) != 1
                || libc::read(hook_fd, (&mut byte as *mut u8).cast(), 1) != 1
                || byte != 2
            {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Err(std::io::Error::from_raw_os_error(libc::EACCES))
        });
    }
    let result = process::exec(&mut command, &selected);
    let restored = selected.map(flags);
    let mut child = worker.join().unwrap();
    let still_contended = matches!(durable::lock(&path, true, false), Err(harbor_db::storage::StorageError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock);
    drop(lease);
    drop(second);
    let acquired = durable::lock(&path, true, false);
    let second_acquired = durable::lock(&second_path, true, false);
    let alive = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        matches!(result, Err(harbor_db::storage::StorageError::Io(error)) if error.raw_os_error() == Some(libc::EACCES))
    );
    assert_eq!(restored, originals);
    assert!(still_contended);
    assert!(alive);
    assert!(
        acquired.is_ok(),
        "unrelated exec retained authority: {:?}",
        acquired.as_ref().err()
    );
    assert!(
        second_acquired.is_ok(),
        "unrelated exec retained second authority: {:?}",
        second_acquired.as_ref().err()
    );

    // A restoration failure must take precedence over the hook's error, and
    // must not prevent restoration of descriptors later in the selection.
    // This raw duplicate has no Rust owner; the hook alone closes it.
    let duplicate = unsafe { libc::fcntl(ordinary.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    assert!(duplicate >= 0);
    let selected = [
        acquired.as_ref().unwrap().fd(),
        duplicate,
        second_acquired.as_ref().unwrap().fd(),
        ordinary.as_raw_fd(),
    ];
    let first_flags = flags(selected[0]);
    let last_flags = flags(selected[2]);
    let mut command = std::process::Command::new("true");
    // SAFETY: close only consumes our unowned duplicate; the failing hook uses
    // no allocation or shared state and does not execute the target command.
    unsafe {
        command.pre_exec(move || {
            if libc::close(duplicate) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Err(std::io::Error::from_raw_os_error(libc::EACCES))
        });
    }
    assert!(
        matches!(process::exec(&mut command, &selected), Err(harbor_db::storage::StorageError::Io(error)) if error.raw_os_error() == Some(libc::EBADF))
    );
    assert_eq!(flags(selected[0]), first_flags);
    assert_eq!(flags(selected[2]), last_flags);
    assert_eq!(flags(selected[3]), 0);
}

#[test]
fn unrelated_parallel_workers_cannot_transiently_retain_closed_authority() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("authority.lock");
    std::fs::write(&path, b"").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut workers = Vec::new();
    for _ in 0..3 {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            for index in 0..100 {
                if index % 3 == 0 {
                    process::execute(&process::CommandSpec::new(vec!["true".into()])).unwrap();
                } else {
                    let mut command = std::process::Command::new("true");
                    if index % 3 == 1 {
                        command
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped());
                        assert!(
                            process::spawn(&mut command)
                                .unwrap()
                                .wait_with_output()
                                .unwrap()
                                .status
                                .success()
                        );
                    } else {
                        assert!(
                            process::spawn(&mut command)
                                .unwrap()
                                .wait()
                                .unwrap()
                                .success()
                        );
                    }
                }
            }
        }));
    }
    barrier.wait();
    let mut rejected = Vec::new();
    for _ in 0..10_000 {
        match durable::lock(&path, false, false) {
            Ok(lease) => {
                drop(lease);
                // Match certification's exclusive-close -> immediate shared
                // writer acquisition, without retry or a serialized test runner.
                match durable::lock(&path, true, false) {
                    Ok(lease) => drop(lease),
                    Err(error) => rejected.push(error.to_string()),
                }
            }
            Err(error) => rejected.push(error.to_string()),
        }
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(
        rejected.is_empty(),
        "unrelated forks retained authority after its owner closed it: {rejected:?}"
    );
    // Child readiness is after exec. It must remain live while the parent
    // closes its lease and a new owner immediately acquires the same inode.
    let lease = durable::lock(&path, false, false).unwrap();
    let ready = root.path().join("unrelated-ready");
    let mut command = std::process::Command::new("sh");
    command
        .args(["-c", "printf ready > \"$1\"; exec sleep 30", "unrelated"])
        .arg(&ready);
    let mut child = process::spawn(&mut command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("unrelated child failed to become ready after exec");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let parent_contended = matches!(
        durable::lock(&path, true, false),
        Err(harbor_db::storage::StorageError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock
    );
    drop(lease);
    let reacquired = durable::lock(&path, true, false);
    let child_still_alive = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        parent_contended,
        "genuine parent contention must be rejected"
    );
    assert!(
        child_still_alive,
        "the spawn gate must not serialize child execution"
    );
    assert!(
        reacquired.is_ok(),
        "an unrelated exec retained the parent's authority: {:?}",
        reacquired.as_ref().err()
    );
}

#[test]
fn legacy_hash_codecs_preserve_python_spacing_and_ascii_escaping() {
    let value = json!({"z": [true, null, "é𝄞"], "a": "line\n"});
    assert_eq!(
        codec::encode(&value, false).unwrap(),
        br#"{"a": "line\n", "z": [true, null, "\u00e9\ud834\udd1e"]}"#
    );
    assert_eq!(
        codec::encode(&value, true).unwrap(),
        br#"{"a":"line\n","z":[true,null,"\u00e9\ud834\udd1e"]}"#
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("literal-object.json");
    for literal in [
        br#"{"$serde_json::private::RawValue": "1"}"#.as_slice(),
        br#"{"metadata": {"$serde_json::private::RawValue": "\"0/300000\""}}"#.as_slice(),
    ] {
        fs::write(&path, literal).unwrap();
        let value = durable::read_json(&path).unwrap();
        assert!(value.is_object());
        assert_eq!(codec::encode(&value, false).unwrap(), literal);
        assert_eq!(fs::read(&path).unwrap(), literal);
    }
}

#[test]
fn golden_vectors_match_retained_python_for_numbers_unicode_and_subprocess_newlines() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let vectors = r#"{"numbers":[184467440737095516160,-0,1e0,1.0000000000000001,-0.0,0.0001,0.00001,1e16,1.2345e-100,1.2345e100],"strings":["\u007f","é𝄞","\b\f\t\r\n", "slash/quote\"backslash\\"]}"#;
    let value: serde_json::Value = serde_json::from_str(vectors).unwrap();
    for compact in [false, true] {
        let script = if compact {
            "import json,sys; print(json.dumps(json.load(sys.stdin),sort_keys=True,separators=(',',':')))"
        } else {
            "import json,sys; print(json.dumps(json.load(sys.stdin),sort_keys=True))"
        };
        let mut command = Command::new("python3");
        command
            .args(["-B", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = process::spawn(&mut command).unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(vectors.as_bytes())
            .unwrap();
        let mut expected = child.wait_with_output().unwrap().stdout;
        assert_eq!(expected.pop(), Some(b'\n'));
        assert_eq!(codec::encode(&value, compact).unwrap(), expected);
    }
    assert_eq!(
        process::text(b"one\r\ntwo\rthree\n").unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[test]
fn authority_locks_retain_the_existing_inode_and_reject_redirects() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("lock");
    let shared = durable::lock(&path, true, true).unwrap();
    assert!(durable::lock(&path, false, false).is_err());
    fs::remove_file(&path).unwrap();
    assert!(durable::lock(&path, false, false).is_err());
    drop(shared);
    symlink(root.path().join("other"), &path).unwrap();
    assert!(durable::lock(&path, false, true).is_err());
}

#[test]
fn explicit_configuration_accepts_store_aliases_but_receipts_and_leases_do_not() {
    let root = tempfile::tempdir().unwrap();
    let policy = root.path().join("policy.json");
    let value = json!({"version": 1, "resource": "configuration-fixture"});
    fs::write(&policy, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(durable::read_config_json(&policy).unwrap(), value);
    assert!(durable::immutable_config_path(&policy).is_err());

    // Use a real daemon-owned store object: temporary directories cannot model
    // the root ownership and read-only inode contract of a NixOS /etc target.
    let add = |path: &std::path::Path| {
        let mut command = std::process::Command::new("nix-store");
        command
            .arg("--add")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = process::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "store fixture realization failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    };
    // Nix checks provide a build-time fixture, since their sandbox must never
    // mutate the store. Native host qualification uses the real daemon instead.
    let fixture = option_env!("HARBOR_DB_TEST_CONFIG_FIXTURE").map(std::path::PathBuf::from);
    let stored = if let Some(fixture) = &fixture {
        fs::canonicalize(fixture.join("postgresql.json")).unwrap()
    } else {
        add(&policy)
    };
    assert_eq!(durable::read_config_json(&stored).unwrap(), value);
    let generation = root.path().join("generation");
    fs::create_dir(&generation).unwrap();
    symlink(&stored, generation.join("postgresql.json")).unwrap();
    symlink(&policy, generation.join("mutable.json")).unwrap();
    symlink("/dev/null", generation.join("special.json")).unwrap();
    let generation = fixture.unwrap_or_else(|| add(&generation));
    // Model both an /etc parent directory alias and a final manifest alias,
    // including a store-internal redirect to another immutable store output.
    let etc = root.path().join("etc");
    symlink(&generation, &etc).unwrap();
    let alias = etc.join("postgresql.json");
    assert_eq!(durable::read_config_json(&alias).unwrap(), value);
    assert_eq!(durable::immutable_config_path(&alias).unwrap(), stored);
    assert!(durable::read_json(&alias).is_err());
    assert!(durable::lock(&alias, true, false).is_err());
    assert!(durable::read_config_json(&etc.join("mutable.json")).is_err());
    assert!(durable::read_config_json(&etc.join("special.json")).is_err());

    // A read-only mutable-location target is still not an immutable policy.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o444)).unwrap();
    let mutable_alias = root.path().join("mutable-alias.json");
    symlink(&policy, &mutable_alias).unwrap();
    assert!(durable::read_config_json(&mutable_alias).is_err());
    let mutable_parent = root.path().join("mutable-parent");
    symlink(root.path(), &mutable_parent).unwrap();
    assert!(durable::read_config_json(&mutable_parent.join("policy.json")).is_err());
}

#[test]
fn explicit_configuration_rejects_special_files_and_symlink_cycles_without_blocking() {
    let root = tempfile::tempdir().unwrap();
    assert!(durable::read_config_json(root.path()).is_err());
    assert!(durable::read_config_json(std::path::Path::new("/dev/null")).is_err());
    let fifo = root.path().join("policy.fifo");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated pathname; mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(durable::read_config_json(&fifo).is_err());
    let cycle = root.path().join("cycle.json");
    symlink(&cycle, &cycle).unwrap();
    assert!(durable::read_config_json(&cycle).is_err());
}

#[test]
fn publishing_rejects_special_storage_and_does_not_replace_a_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    symlink("/etc/passwd", source.join("redirect")).unwrap();
    let destination = root.path().join("destination");
    assert!(durable::publish_tree(&source, &destination).is_err());
    assert!(source.exists());
    fs::remove_file(source.join("redirect")).unwrap();
    fs::write(source.join("record"), b"acknowledged").unwrap();
    durable::publish_tree(&source, &destination).unwrap();
    assert_eq!(
        fs::read(destination.join("record")).unwrap(),
        b"acknowledged"
    );
    fs::write(&source, b"retained").unwrap();
    assert!(durable::publish_file(&source, &destination).is_err());
    assert_eq!(fs::read(source).unwrap(), b"retained");
}

#[test]
fn receipt_execution_drains_diagnostics_without_persisting_them() {
    let spec = process::CommandSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "head -c 2000000 /dev/zero >&2; printf '{}'".into(),
    ]);
    assert_eq!(process::execute(&spec).unwrap(), b"{}");
}

#[test]
fn receipt_eof_does_not_imply_success_or_disable_the_deadline() {
    let mut spec = process::CommandSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "exec 1>&- 2>&-; sleep 30".into(),
    ]);
    spec.timeout = Duration::from_millis(100);
    let start = Instant::now();
    assert!(
        process::execute(&spec)
            .unwrap_err()
            .to_string()
            .contains("execution limit")
    );
    assert!(start.elapsed() < Duration::from_secs(8));
}

#[test]
fn oversized_receipts_and_nonzero_workers_do_not_leak_diagnostics() {
    let mut spec = process::CommandSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "head -c 100000 /dev/zero".into(),
    ]);
    spec.maximum_output = 1024;
    assert!(
        process::execute(&spec)
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
    spec.argv = vec![
        "sh".into(),
        "-c".into(),
        "echo fixture-secret >&2; exit 1".into(),
    ];
    let error = process::execute(&spec).unwrap_err().to_string();
    assert!(error.contains("diagnostics suppressed"));
    assert!(!error.contains("fixture-secret"));
}
