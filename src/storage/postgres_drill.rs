//! Logical restoration using a private, disposable local PostgreSQL endpoint.
use super::{Result, durable, invalid, pg_core, process};
use std::{
    fs,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    time::{Duration, Instant},
};

/// A PID file is an untrusted reference, never signalling authority. A pidfd
/// pins the inspected process even if it exits and its numeric PID is reused.
pub(crate) fn cleanup(cluster: &Path, socket: &Path, deadline: Instant) -> Result<()> {
    let path = cluster.join("postmaster.pid");
    let mut file = match durable::open_regular(&path, false) {
        Ok(file) => file,
        Err(super::StorageError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let info = file.metadata()?;
    if !pg_core::unredirected(cluster)?
        || info.uid() != unsafe { libc::getuid() }
        || info.nlink() != 1
        || info.mode() & 0o077 != 0
    {
        return Err(invalid(
            "disposable PID carrier must be owned, private and unredirected",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref().take(4097).read_to_end(&mut bytes)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("invalid disposable PID carrier"))?;
    let lines: Vec<_> = text.lines().collect();
    let pid = lines
        .first()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|pid| *pid > 1)
        .ok_or_else(|| invalid("invalid disposable PID carrier"))?;
    let started = lines
        .get(2)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|started| *started > 0)
        .ok_or_else(|| invalid("invalid disposable PID carrier"))?;
    if bytes.len() > 4096
        || lines.get(1)
            != Some(
                &cluster
                    .to_str()
                    .ok_or_else(|| invalid("invalid disposable cluster path"))?,
            )
        || lines.get(3) != Some(&"55439")
        || lines.get(4)
            != Some(
                &socket
                    .to_str()
                    .ok_or_else(|| invalid("invalid disposable socket path"))?,
            )
    {
        return Err(invalid(
            "disposable PID carrier does not identify its workspace",
        ));
    }
    let unchanged = || -> Result<bool> {
        let input = durable::open_regular(&path, false)?;
        let current = input.metadata()?;
        if current.dev() != info.dev() || current.ino() != info.ino() {
            return Ok(false);
        }
        let mut contents = Vec::new();
        input.take(4097).read_to_end(&mut contents)?;
        Ok(contents == bytes)
    };
    let retire = || -> Result<()> {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return durable::sync_directory(cluster);
            }
            Err(error) => return Err(error.into()),
            Ok(current) if current.dev() != info.dev() || current.ino() != info.ino() => {
                return Err(invalid("disposable PID carrier changed during cleanup"));
            }
            Ok(_) => {}
        }
        if !unchanged()? {
            return Err(invalid("disposable PID carrier changed during cleanup"));
        }
        fs::remove_file(&path)?;
        durable::sync_directory(cluster)
    };
    // SAFETY: pidfd_open takes a positive numeric PID and no pointer arguments.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            retire()
        } else {
            Err(error.into())
        };
    }
    // SAFETY: the successful syscall returned a newly owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    let exited = || -> Result<bool> {
        let mut event = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event is one initialized, writable pollfd.
        if unsafe { libc::poll(&mut event, 1, 0) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(event.revents & libc::POLLIN != 0)
    };
    if exited()? {
        return retire();
    }
    let inspect = || -> Result<bool> {
        let proc = Path::new("/proc").join(pid.to_string());
        let stat = fs::read_to_string(proc.join("stat"))?;
        let fields: Vec<_> = stat
            .rsplit_once(") ")
            .ok_or_else(|| invalid("invalid postmaster process identity"))?
            .1
            .split_whitespace()
            .collect();
        let ticks = fields
            .get(19)
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| invalid("invalid postmaster process start time"))?;
        let boot = fs::read_to_string("/proc/stat")?
            .lines()
            .find_map(|line| line.strip_prefix("btime ")?.parse::<i64>().ok())
            .ok_or_else(|| invalid("missing kernel boot time"))?;
        // SAFETY: sysconf takes a constant and does not borrow memory.
        let frequency = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if frequency <= 0 {
            return Err(invalid("missing kernel clock frequency"));
        }
        let executable = fs::read_link(proc.join("exe"))?;
        let argv = fs::read(proc.join("cmdline"))?;
        let postgres = executable
            .file_name()
            .is_some_and(|name| name == "postgres")
            || (executable
                .file_name()
                .is_some_and(|name| name.as_bytes().starts_with(b"qemu-"))
                && argv.split(|b| *b == 0).any(|arg| {
                    Path::new(std::ffi::OsStr::from_bytes(arg))
                        .file_name()
                        .is_some_and(|name| name == "postgres")
                }));
        Ok(postgres
            && fs::metadata(&proc)?.uid() == info.uid()
            && fs::read_link(proc.join("cwd"))? == cluster
            && fields.get(2).and_then(|value| value.parse::<i32>().ok()) == Some(pid)
            && fields.get(3).and_then(|value| value.parse::<i32>().ok()) == Some(pid)
            && (boot + ticks / frequency - started).abs() <= 2)
    };
    let valid = inspect();
    if exited()? {
        return retire();
    }
    if !valid? {
        return Err(invalid(
            "disposable PID references a different process; cleanup refused",
        ));
    }
    if !unchanged()? {
        return Err(invalid("disposable PID carrier changed during cleanup"));
    }
    // SAFETY: the owned pidfd pins the inspected postmaster; no pointer is used.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            descriptor.as_raw_fd(),
            libc::SIGINT,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    while !exited()? {
        if Instant::now() >= deadline {
            return Err(invalid("disposable restore exceeded its execution limit"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    retire()
}

pub(crate) fn run(
    package: &Path,
    program: &str,
    args: Vec<String>,
    deadline: Instant,
) -> Result<()> {
    let mut argv = vec![package.join("bin").join(program).display().to_string()];
    argv.extend(args);
    let mut spec = pg_core::command(argv, true);
    spec.timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| invalid("disposable restore exceeded its execution limit"))?;
    process::execute(&spec)
        .map_err(|_| invalid(format!("disposable PostgreSQL {program} failed")))?;
    Ok(())
}

/// Create `workspace/cluster` and start it on `workspace/socket` (which must
/// already exist), reachable only through that socket on the fixed drill port.
/// `superuser` names the role `initdb` creates; `None` keeps `initdb`'s default
/// of the account running the engine. `settings` are extra `name=value` server
/// settings. Restore and rehearsal share this so both leave the same PID
/// carrier for `cleanup`.
pub(crate) fn start_cluster(
    package: &Path,
    workspace: &Path,
    superuser: Option<&str>,
    settings: &[&str],
    deadline: Instant,
) -> Result<()> {
    let data = workspace.join("cluster").display().to_string();
    let socket = workspace.join("socket");
    let mut initdb = vec![
        "-D".into(),
        data.clone(),
        "--locale=C".into(),
        "--encoding=UTF8".into(),
        "--auth=trust".into(),
    ];
    initdb.extend(
        superuser
            .iter()
            .flat_map(|superuser| ["--username".into(), superuser.to_string()]),
    );
    run(package, "initdb", initdb, deadline)?;
    let mut options = format!(
        "-k {} -p 55439 -c listen_addresses=",
        shell_quote(&socket.display().to_string())
    );
    for setting in settings {
        options.push_str(&format!(" -c {}", shell_quote(setting)));
    }
    run(
        package,
        "pg_ctl",
        vec![
            "-D".into(),
            data,
            "-l".into(),
            "/dev/null".into(),
            "-o".into(),
            options,
            "-w".into(),
            "start".into(),
        ],
        deadline,
    )
}

pub fn operate(
    package: &Path,
    command: &str,
    backup: &Path,
    workspace: &Path,
    dump: &str,
) -> Result<()> {
    operate_with_timeout(
        package,
        command,
        backup,
        workspace,
        dump,
        timeout_seconds(None)?,
    )
}

/// The owning application adapter supplies its contract budget. Standalone
/// callers use the same 1800-second default or an explicit bounded CLI value.
pub fn timeout_seconds(explicit: Option<u64>) -> Result<u64> {
    let seconds = match explicit {
        Some(seconds) => seconds,
        None => match std::env::var("HARBOR_DB_APPLICATION_TIMEOUT_SECONDS") {
            Ok(value) => value
                .parse()
                .map_err(|_| invalid("invalid disposable restore timeout"))?,
            Err(std::env::VarError::NotPresent) => 1800,
            Err(_) => return Err(invalid("invalid disposable restore timeout")),
        },
    };
    if !(1..=86400).contains(&seconds) {
        return Err(invalid("invalid disposable restore timeout"));
    }
    Ok(seconds)
}

pub fn operate_with_timeout(
    package: &Path,
    command: &str,
    backup: &Path,
    workspace: &Path,
    dump: &str,
    timeout: u64,
) -> Result<()> {
    let timeout = timeout_seconds(Some(timeout))?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    if !pg_core::unredirected(workspace)? || !pg_core::unredirected(backup)? {
        return Err(invalid(
            "disposable restore paths must be absolute and unredirected",
        ));
    }
    let info = fs::metadata(workspace)?;
    // SAFETY: getuid has no preconditions or borrowed pointers.
    if info.uid() != unsafe { libc::getuid() } || info.mode() & 0o077 != 0 {
        return Err(invalid("disposable workspace must be owned and private"));
    }
    if Path::new(dump).file_name().and_then(|s| s.to_str()) != Some(dump) {
        return Err(invalid("dump must be a direct backup child"));
    }
    let cluster = workspace.join("cluster");
    let socket = workspace.join("socket");
    if command == "cleanup" {
        return cleanup(&cluster, &socket, deadline);
    }
    if command != "restore" {
        return Err(invalid("unsupported disposable restore command"));
    }
    if cluster.exists() || socket.exists() {
        return Err(invalid("disposable restore requires a new workspace"));
    }
    fs::DirBuilder::new().mode(0o700).create(&socket)?;
    let result = (|| {
        start_cluster(package, workspace, None, &[], deadline)?;
        let mut endpoint = vec![
            "-h".into(),
            socket.display().to_string(),
            "-p".into(),
            "55439".into(),
        ];
        let mut created = endpoint.clone();
        created.push("harbor_restore".into());
        run(package, "createdb", created, deadline)?;
        let mut restore = vec![
            "--exit-on-error".into(),
            "--no-owner".into(),
            "--no-acl".into(),
        ];
        restore.append(&mut endpoint);
        restore.extend([
            "-d".into(),
            "harbor_restore".into(),
            backup.join(dump).display().to_string(),
        ]);
        run(package, "pg_restore", restore, deadline)
    })();
    if result.is_err() {
        // Cleanup has its own bounded attempt after the restore budget expires.
        operate_with_timeout(
            package,
            "cleanup",
            backup,
            workspace,
            dump,
            timeout.min(120),
        )?;
    }
    result
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
