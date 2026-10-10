#![cfg(target_os = "linux")]

use harbor_db::storage::process::{self, CommandSpec};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn python() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("python3"))
        .find(|path| path.is_file())
        .unwrap()
}

fn capture(spec: &CommandSpec) -> std::process::Output {
    use std::process::{Command, Stdio};
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.environment.as_ref().unwrap())
        .stdin(Stdio::null())
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap());
    let mut child = process::spawn(&mut command).unwrap();
    let deadline = Instant::now() + spec.timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("drill fixture exceeded its outer deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    std::process::Output {
        status,
        stdout: fs::read(stdout.path()).unwrap(),
        stderr: fs::read(stderr.path()).unwrap(),
    }
}

struct Fixture {
    root: tempfile::TempDir,
    package: PathBuf,
    backup: PathBuf,
    clusters: Vec<PathBuf>,
}

impl Fixture {
    fn pg(&self, program: &str, arguments: Vec<String>) -> std::process::Output {
        let mut spec = CommandSpec::new(
            std::iter::once(self.package.join("bin").join(program).display().to_string())
                .chain(arguments)
                .collect(),
        );
        spec.environment = Some(BTreeMap::from([("LC_ALL".into(), "C".into())]));
        spec.timeout = Duration::from_secs(30);
        process::run(&spec).unwrap()
    }

    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let package = PathBuf::from(
            std::env::var_os("HARBOR_DB_TEST_POSTGRES").expect("qualified PostgreSQL package"),
        );
        let backup = root.path().join("backup");
        fs::create_dir(&backup).unwrap();
        let source = root.path().join("source");
        let socket = root.path().join("source-socket");
        fs::create_dir(&socket).unwrap();
        let fixture = Self {
            root,
            package,
            backup,
            clusters: vec![source.clone()],
        };
        for (program, argv) in [
            (
                "initdb",
                vec![
                    "-D".into(),
                    source.display().to_string(),
                    "--locale=C".into(),
                    "--encoding=UTF8".into(),
                    "--auth=trust".into(),
                ],
            ),
            (
                "pg_ctl",
                vec![
                    "-D".into(),
                    source.display().to_string(),
                    "-l".into(),
                    "/dev/null".into(),
                    "-o".into(),
                    format!("-k {} -p 55440 -c listen_addresses=", socket.display()),
                    "-w".into(),
                    "start".into(),
                ],
            ),
            (
                "psql",
                vec![
                    "-h".into(),
                    socket.display().to_string(),
                    "-p".into(),
                    "55440".into(),
                    "-d".into(),
                    "postgres".into(),
                    "-c".into(),
                    "CREATE TABLE records(id int, revision int); INSERT INTO records VALUES (1,7)"
                        .into(),
                ],
            ),
            (
                "pg_dump",
                vec![
                    "-h".into(),
                    socket.display().to_string(),
                    "-p".into(),
                    "55440".into(),
                    "-d".into(),
                    "postgres".into(),
                    "--format=custom".into(),
                    "--file".into(),
                    fixture.backup.join("database.dump").display().to_string(),
                ],
            ),
            (
                "pg_ctl",
                vec![
                    "-D".into(),
                    source.display().to_string(),
                    "-m".into(),
                    "fast".into(),
                    "-w".into(),
                    "stop".into(),
                ],
            ),
        ] {
            let output = fixture.pg(program, argv);
            assert!(output.status.success(), "{output:?}");
        }
        fixture
    }

    fn workspace(&mut self, name: &str) -> PathBuf {
        let workspace = self.root.path().join(name);
        fs::create_dir(&workspace).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
        self.clusters.push(workspace.join("cluster"));
        workspace
    }

    fn delayed_package(&self, name: &str, seconds: u64) -> PathBuf {
        let package = self.root.path().join(name);
        let bin = package.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for program in ["initdb", "pg_ctl", "createdb"] {
            let wrapper = bin.join(program);
            let executable = self.package.join("bin").join(program).display().to_string();
            fs::write(&wrapper, format!("#!{}\nimport os,sys\nos.execv({executable:?}, [{executable:?}, *sys.argv[1:]])\n", python().display())).unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let restore = bin.join("pg_restore");
        // Existence is the cancellation barrier. Publish only after the PID
        // carrier is closed so the observer cannot read a newly created empty file.
        fs::write(&restore, format!("#!{}\nimport os,pathlib,sys,time\nmarker = pathlib.Path({:?})\npending = marker.with_suffix('.pending')\npending.write_text(str(os.getpid()))\npending.replace(marker)\ntime.sleep({seconds})\nos.execv({:?}, [\"pg_restore\", *sys.argv[1:]])\n", python().display(), package.join("restore-started").display().to_string(), self.package.join("bin/pg_restore").display().to_string())).unwrap();
        fs::set_permissions(&restore, fs::Permissions::from_mode(0o700)).unwrap();
        package
    }

    fn drill(
        &self,
        native: bool,
        package: &Path,
        operation: &str,
        workspace: &Path,
        seconds: u64,
    ) -> CommandSpec {
        let mut argv = if native {
            vec![env!("CARGO_BIN_EXE_harbor-db-postgres-drill").into()]
        } else {
            vec![
                python().display().to_string(),
                "-B".into(),
                "-m".into(),
                "harbor_db.postgres_drill".into(),
            ]
        };
        argv.extend([
            "--package".into(),
            package.display().to_string(),
            operation.into(),
            self.backup.display().to_string(),
            workspace.display().to_string(),
        ]);
        let mut spec = CommandSpec::new(argv);
        spec.environment = Some(BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            (
                "HARBOR_DB_APPLICATION_TIMEOUT_SECONDS".into(),
                seconds.to_string(),
            ),
            ("LC_ALL".into(), "C".into()),
        ]));
        spec.timeout = Duration::from_secs(150);
        spec
    }

    fn stopped(&self, workspace: &Path) {
        assert!(!workspace.join("cluster/postmaster.pid").exists());
        assert!(!workspace.join("socket/.s.PGSQL.55439").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for cluster in &self.clusters {
            if cluster.join("postmaster.pid").exists() {
                let _ = self.pg(
                    "pg_ctl",
                    vec![
                        "-D".into(),
                        cluster.display().to_string(),
                        "-m".into(),
                        "immediate".into(),
                        "-w".into(),
                        "stop".into(),
                    ],
                );
            }
        }
    }
}

#[test]
fn owning_deadline_aborts_restore_and_allows_peer_cleanup() {
    // initdb must finish under registered AArch64 emulation before the delayed
    // restore starts. Keep the restore delay longer than the owning budget so
    // both engines still have to enforce the contract rather than finish it.
    const BUDGET: u64 = 30;
    let mut fixture = Fixture::new();
    let original = fs::read(fixture.backup.join("database.dump")).unwrap();
    for native in [true, false] {
        let workspace = fixture.workspace(if native { "native" } else { "python" });
        let package = fixture.delayed_package(
            if native {
                "native-package"
            } else {
                "python-package"
            },
            BUDGET + 1,
        );
        let started = Instant::now();
        let output = capture(&fixture.drill(native, &package, "restore", &workspace, BUDGET));
        assert!(
            !output.status.success(),
            "a restore may not exceed its owning deadline: {output:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(BUDGET + 10));
        assert!(
            package.join("restore-started").is_file(),
            "restore worker was never reached: {output:?}"
        );
        fixture.stopped(&workspace);
        assert!(
            process::execute(&fixture.drill(
                !native,
                &fixture.package,
                "cleanup",
                &workspace,
                BUDGET
            ))
            .is_ok()
        );
        assert_eq!(
            fs::read(fixture.backup.join("database.dump")).unwrap(),
            original
        );
    }
}

#[test]
fn restores_outlive_the_old_120_second_probe_limit_in_both_engines() {
    let mut fixture = Fixture::new();
    let native_workspace = fixture.workspace("long-native");
    let python_workspace = fixture.workspace("long-python");
    let native_package = fixture.delayed_package("long-native-package", 121);
    let python_package = fixture.delayed_package("long-python-package", 121);
    let specs = [
        fixture.drill(true, &native_package, "restore", &native_workspace, 1800),
        fixture.drill(false, &python_package, "restore", &python_workspace, 1800),
    ];
    let began = Instant::now();
    let outputs = std::thread::scope(|scope| {
        let workers = specs
            .iter()
            .map(|spec| scope.spawn(move || capture(spec)))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(began.elapsed() >= Duration::from_secs(121));
    for (native, workspace, output) in [
        (true, &native_workspace, &outputs[0]),
        (false, &python_workspace, &outputs[1]),
    ] {
        assert!(output.status.success(), "{output:?}");
        let records = fixture.pg(
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
        assert!(records.status.success(), "{records:?}");
        assert_eq!(records.stdout, b"1|7\n");
        assert!(
            capture(&fixture.drill(!native, &fixture.package, "cleanup", workspace, 10))
                .status
                .success()
        );
        fixture.stopped(workspace);
    }
}

#[test]
fn invalid_budgets_reject_before_mutation_and_explicit_budget_overrides_inheritance() {
    let mut fixture = Fixture::new();
    for native in [true, false] {
        let workspace = fixture.workspace(if native {
            "limits-native"
        } else {
            "limits-python"
        });
        for invalid in ["0", "86401", "not-a-number"] {
            let mut spec = fixture.drill(native, &fixture.package, "restore", &workspace, 10);
            spec.environment.as_mut().unwrap().insert(
                "HARBOR_DB_APPLICATION_TIMEOUT_SECONDS".into(),
                invalid.into(),
            );
            assert!(!capture(&spec).status.success());
            assert_eq!(fs::read_dir(&workspace).unwrap().count(), 0);
        }
        let package = fixture.delayed_package(
            if native {
                "explicit-native-package"
            } else {
                "explicit-python-package"
            },
            2,
        );
        let mut spec = fixture.drill(native, &package, "restore", &workspace, 1);
        spec.argv.extend(["--timeout-seconds".into(), "10".into()]);
        assert!(capture(&spec).status.success());
        assert!(
            capture(&fixture.drill(!native, &fixture.package, "cleanup", &workspace, 10))
                .status
                .success()
        );
        fixture.stopped(&workspace);
    }
}

#[test]
fn cancelled_restore_retains_private_state_for_explicit_peer_cleanup() {
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        process::{Command, Stdio},
    };
    let mut fixture = Fixture::new();
    let original = fs::read(fixture.backup.join("database.dump")).unwrap();
    for native in [true, false] {
        let workspace = fixture.workspace(if native {
            "cancel-native"
        } else {
            "cancel-python"
        });
        let package = fixture.delayed_package(
            if native {
                "cancel-native-package"
            } else {
                "cancel-python-package"
            },
            60,
        );
        let spec = fixture.drill(native, &package, "restore", &workspace, 1800);
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .env_clear()
            .envs(spec.environment.as_ref().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = process::spawn(&mut command).unwrap();
        let marker = package.join("restore-started");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.is_file() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("restore did not reach the cancellation barrier");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
        // SAFETY: pin the live controlled wrapper before cancelling its coordinator;
        // the pidfd binds signalling to that process even after parent death.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        assert!(fd >= 0);
        let worker = unsafe { OwnedFd::from_raw_fd(fd) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(workspace.join("cluster/postmaster.pid").is_file());
        // Stop only the pinned fixture worker; cleanup is performed by the peer CLI.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    worker.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
        assert!(
            capture(&fixture.drill(!native, &fixture.package, "cleanup", &workspace, 10))
                .status
                .success()
        );
        fixture.stopped(&workspace);
        assert_eq!(
            fs::read(fixture.backup.join("database.dump")).unwrap(),
            original
        );
        assert!(workspace.join("cluster/global/pg_control").is_file());
    }
}
