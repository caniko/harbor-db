#![cfg(unix)]

use harbor_db::storage::process::{self, CommandSpec};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

fn python() -> PathBuf {
    let path = std::env::var_os("HARBOR_DB_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH").expect("approved PATH"))
                .map(|directory| directory.join("python3"))
                .find(|path| path.is_file())
                .expect("approved environment provides Python")
        });
    assert!(path.is_absolute(), "Python requires an absolute path");
    path
}

fn environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "PYTHONPATH".into(),
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        ),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("PGCLIENTENCODING".into(), "UTF8".into()),
    ])
}

struct Worker(Option<Child>);
impl Drop for Worker {
    fn drop(&mut self) {
        let Some(child) = &mut self.0 else {
            return;
        };
        // The child remains owned until cancellation; kill its subprocesses too.
        // SAFETY: the unreaped child's PID cannot have been recycled.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    }
}

fn capture(spec: &CommandSpec) -> Output {
    // File-backed capture avoids inherited pipes delaying EOF after pg_ctl start.
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.environment.as_ref().unwrap())
        .stdin(Stdio::null())
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .process_group(0);
    let mut worker = Worker(Some(process::spawn(&mut command).unwrap()));
    let deadline = Instant::now() + spec.timeout;
    let status = loop {
        if let Some(status) = worker.0.as_mut().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "worker deadline: {:?}",
            spec.argv
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    // try_wait reaped the child; do not signal a potentially recycled PID.
    worker.0.take();
    Output {
        status,
        stdout: fs::read(stdout.path()).unwrap(),
        stderr: fs::read(stderr.path()).unwrap(),
    }
}

fn pg(package: &Path, program: &str, args: &[&str]) -> Output {
    let mut spec = CommandSpec::new(
        std::iter::once(package.join("bin").join(program).display().to_string())
            .chain(args.iter().map(|arg| (*arg).into()))
            .collect(),
    );
    spec.environment = Some(environment());
    spec.timeout = Duration::from_secs(30);
    capture(&spec)
}

fn successful(output: Output) -> Vec<u8> {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    output.stdout
}

fn drill(native: bool, package: &Path, operation: &str, backup: &Path, workspace: &Path) -> Output {
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
        backup.display().to_string(),
        workspace.display().to_string(),
    ]);
    let mut spec = CommandSpec::new(argv);
    spec.environment = Some(environment());
    spec.timeout = Duration::from_secs(90);
    let output = capture(&spec);
    if output.status.success() {
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
    }
    output
}

struct Clusters {
    package: PathBuf,
    directories: Vec<PathBuf>,
}
impl Drop for Clusters {
    fn drop(&mut self) {
        for directory in &self.directories {
            if directory.join("postmaster.pid").exists() {
                for mode in ["fast", "immediate"] {
                    let mut spec = CommandSpec::new(vec![
                        self.package.join("bin/pg_ctl").display().to_string(),
                        "-D".into(),
                        directory.display().to_string(),
                        "-m".into(),
                        mode.into(),
                        "-w".into(),
                        "stop".into(),
                    ]);
                    spec.environment = Some(environment());
                    spec.timeout = Duration::from_secs(30);
                    if process::execute(&spec).is_ok() {
                        break;
                    }
                }
            }
        }
    }
}

fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn record(package: &Path, socket: &Path, port: &str, database: &str) -> serde_json::Value {
    let bytes = successful(pg(
        package,
        "psql",
        &[
            "-X",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            socket.to_str().unwrap(),
            "-p",
            port,
            "-d",
            database,
            "-c",
            "SELECT coalesce(json_agg(json_build_object('id',id,'revision',revision,'body',encode(body,'hex'),'label',label) ORDER BY id),'[]'::json) FROM records",
        ],
    ));
    serde_json::from_slice(&bytes).unwrap()
}

fn carrier(workspace: &Path, running: bool) -> BTreeMap<PathBuf, Vec<u8>> {
    // Neither drill has JSON/receipts: these PostgreSQL files are its persisted carrier.
    let mut names = vec![
        "PG_VERSION",
        "postgresql.conf",
        "postgresql.auto.conf",
        "pg_hba.conf",
        "pg_ident.conf",
        "postmaster.opts",
    ];
    if running {
        names.push("postmaster.pid");
    }
    names
        .into_iter()
        .map(|name| {
            let path = workspace.join("cluster").join(name);
            (path.clone(), fs::read(path).unwrap())
        })
        .collect()
}

fn identity(path: &Path) -> (u64, u64, u32, u32) {
    let metadata = fs::metadata(path).unwrap();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
    )
}

#[test]
fn python_and_native_drill_exchange_restored_clusters_and_cleanup() {
    let package = PathBuf::from(
        std::env::var_os("HARBOR_DB_TEST_POSTGRES").expect("approved PG18 package required"),
    );
    assert!(package.is_absolute() && package.join("bin/pg_dump").is_file());
    assert!(
        String::from_utf8(successful(pg(&package, "postgres", &["--version"])))
            .unwrap()
            .contains(" 18.")
    );
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let source_socket = root.path().join("source-socket");
    let backup = root.path().join("backup");
    let workspaces = [
        root.path().join("python-workspace"),
        root.path().join("native-workspace"),
    ];
    for directory in [&source_socket, &backup, &workspaces[0], &workspaces[1]] {
        private(directory);
    }
    // Register every possible postmaster before any launch, including partial restore failures.
    let _clusters = Clusters {
        package: package.clone(),
        directories: vec![
            source.clone(),
            workspaces[0].join("cluster"),
            workspaces[1].join("cluster"),
        ],
    };
    successful(pg(
        &package,
        "initdb",
        &[
            "-D",
            source.to_str().unwrap(),
            "--locale=C",
            "--encoding=UTF8",
            "--auth=trust",
        ],
    ));
    successful(pg(
        &package,
        "pg_ctl",
        &[
            "-D",
            source.to_str().unwrap(),
            "-l",
            "/dev/null",
            "-o",
            &format!(
                "-k {} -p 55440 -c listen_addresses=",
                source_socket.display()
            ),
            "-w",
            "start",
        ],
    ));
    // Bytea includes NUL, non-UTF8, and UTF8 Unicode bytes; SQL is ordinary valid PG SQL.
    let body = "0001275c7fff80e99baaf09f9a80";
    let expected = json!([{"id":1,"revision":7,"body":body,"label":"雪 🚀 ' \\"}]);
    successful(pg(
        &package,
        "psql",
        &[
            "-X",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            source_socket.to_str().unwrap(),
            "-p",
            "55440",
            "-d",
            "postgres",
            "-c",
            &format!(
                "CREATE TABLE records(id int PRIMARY KEY, revision bigint NOT NULL, body bytea NOT NULL, label text NOT NULL); INSERT INTO records VALUES (1,7,decode('{body}','hex'),$label$雪 🚀 ' \\$label$)"
            ),
        ],
    ));
    assert_eq!(
        record(&package, &source_socket, "55440", "postgres"),
        expected
    );
    successful(pg(
        &package,
        "pg_dump",
        &[
            "-h",
            source_socket.to_str().unwrap(),
            "-p",
            "55440",
            "-d",
            "postgres",
            "--format=custom",
            "--file",
            backup.join("database.dump").to_str().unwrap(),
        ],
    ));
    let dump = fs::read(backup.join("database.dump")).unwrap();
    assert!(dump.starts_with(b"PGDMP") && dump.len() > 100);
    successful(pg(
        &package,
        "pg_ctl",
        &["-D", source.to_str().unwrap(), "-m", "fast", "-w", "stop"],
    ));
    assert!(!source.join("postmaster.pid").exists());

    for (native, workspace) in [false, true].into_iter().zip(&workspaces) {
        assert_eq!(fs::read_dir(workspace).unwrap().count(), 0);
        let output = drill(native, &package, "restore", &backup, workspace);
        assert!(successful(output).is_empty());
        let mut entries: Vec<_> = fs::read_dir(workspace)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, ["cluster", "socket"]);
        let socket = workspace.join("socket");
        let cluster = workspace.join("cluster");
        let dirs = (identity(&cluster), identity(&socket));
        let before = carrier(workspace, true);
        assert_eq!(before[&cluster.join("PG_VERSION")], b"18\n");
        let pid = String::from_utf8(before[&cluster.join("postmaster.pid")].clone()).unwrap();
        let lines: Vec<_> = pid.lines().collect();
        assert_eq!(lines[1], cluster.to_str().unwrap());
        assert_eq!(lines[3], "55439");
        assert_eq!(lines[4], socket.to_str().unwrap());
        assert_eq!(
            record(&package, &socket, "55439", "harbor_restore"),
            expected
        );

        let refused = drill(!native, &package, "restore", &backup, workspace);
        assert_eq!(refused.status.code(), Some(1), "{refused:?}");
        assert!(refused.stdout.is_empty());
        assert_eq!(
            refused.stderr,
            b"harbor-db-postgres-drill: disposable restore requires a new workspace\n"
        );
        assert_eq!(carrier(workspace, true), before);
        assert_eq!((identity(&cluster), identity(&socket)), dirs);
        assert_eq!(
            record(&package, &socket, "55439", "harbor_restore"),
            expected
        );

        assert!(successful(drill(!native, &package, "cleanup", &backup, workspace)).is_empty());
        assert!(!cluster.join("postmaster.pid").exists());
        assert_eq!(
            pg(
                &package,
                "pg_ctl",
                &["-D", cluster.to_str().unwrap(), "status"]
            )
            .status
            .code(),
            Some(3)
        );
        assert!(!socket.join(".s.PGSQL.55439").exists());
        let retained = carrier(workspace, false);
        for (path, bytes) in &retained {
            assert_eq!(before[path], *bytes);
        }
        assert_eq!((identity(&cluster), identity(&socket)), dirs);
        assert!(cluster.join("global/pg_control").is_file());
        assert!(cluster.join("base").is_dir());
        assert!(successful(drill(native, &package, "cleanup", &backup, workspace)).is_empty());
        assert_eq!(carrier(workspace, false), retained);
        assert!(!cluster.join("postmaster.pid").exists());
        assert_eq!(fs::read(backup.join("database.dump")).unwrap(), dump);
        assert!(!source.join("postmaster.pid").exists());
    }
}
