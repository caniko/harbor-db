//! Bounded workers retain explicitly supplied leases and discard diagnostics.
use super::{Result, invalid};
use std::sync::{Mutex, MutexGuard};

// An unrelated fork temporarily copies even close-on-exec flock descriptors.
// Coordinate acquisition and release with the fork/exec handshake, never worker execution.
static FORK_GATE: Mutex<()> = Mutex::new(());

pub(crate) fn fork_guard() -> Result<MutexGuard<'static, ()>> {
    FORK_GATE
        .lock()
        .map_err(|_| invalid("worker spawn coordination poisoned"))
}

/// Spawn while coordinating the fork/exec handshake with authority leases.
/// Waiting on the returned child does not retain the coordination gate.
pub fn spawn(command: &mut Command) -> Result<std::process::Child> {
    let _guard = fork_guard()?;
    // Require Rust's fork/exec error-pipe handshake instead of posix_spawn's
    // implicit vfork barrier. QEMU user-mode emulates CLONE_VFORK with fork,
    // so that fast path can return before exec closes copied lease descriptors.
    // The gate must remain held until those unrelated copies are gone.
    // SAFETY: this no-op child hook allocates nothing and touches no shared state.
    unsafe {
        command.pre_exec(|| Ok(()));
    }
    Ok(command.spawn()?)
}

/// Replace this process while enabling inheritance for explicitly selected leases.
/// Selected descriptors must remain valid throughout the call. All command
/// construction and validation must precede handover. On failure, restore every
/// original descriptor flag before allowing coordinated forks again; a restoration
/// error takes precedence over the exec error and means recovery is incomplete.
pub fn exec(command: &mut Command, leases: &[RawFd]) -> Result<()> {
    let _guard = fork_guard()?;
    // Read every original flag before changing any descriptor.
    let flags = leases
        .iter()
        .map(|&fd| {
            // SAFETY: fcntl examines descriptor flags; invalid descriptors fail.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok((fd, flags))
            }
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let error = (|| {
        for &(fd, flags) in &flags {
            // SAFETY: the caller retains these descriptors through handover.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
                return std::io::Error::last_os_error();
            }
        }
        command.exec()
    })();
    let mut restoration_error = None;
    for &(fd, flags) in &flags {
        // Attempt all restorations even if an earlier descriptor fails.
        // SAFETY: fcntl updates flags and reports an invalid descriptor as an error.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
            let error = std::io::Error::last_os_error();
            if restoration_error.is_none() {
                restoration_error = Some(error);
            }
        }
    }
    Err(restoration_error.unwrap_or(error).into())
}

/// Preserve a legacy diagnostic-bearing command contract while coordinating its
/// fork with persistent authority leases. Waiting does not hold the spawn gate.
pub(crate) fn output(command: &mut Command) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(spawn(command)?.wait_with_output()?)
}
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    pub environment: Option<BTreeMap<String, String>>,
    pub cwd: Option<PathBuf>,
    pub leases: Vec<RawFd>,
    pub identity: Option<Identity>,
    pub timeout: Duration,
    pub maximum_output: usize,
    pub input: Option<Vec<u8>>,
}

impl CommandSpec {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            environment: None,
            cwd: None,
            leases: vec![],
            identity: None,
            timeout: Duration::from_secs(60),
            maximum_output: 1024 * 1024,
            input: None,
        }
    }
}

/// Resolve the complete service identity before any child-side privilege change.
pub fn account(name: &str) -> Result<Identity> {
    use std::ffi::CString;
    let name = CString::new(name).map_err(|_| invalid("NUL in account name"))?;
    let mut buffer = vec![0u8; 65536];
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: reentrant lookup receives live writable buffers and returns pointers
    // into that buffer. All used account fields are copied before it is dropped.
    let status = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut record,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status).into());
    }
    if found.is_null() {
        return Err(invalid("unknown service account"));
    }
    let mut count = 32;
    let mut groups = vec![0; count as usize];
    if unsafe {
        libc::getgrouplist(
            name.as_ptr(),
            record.pw_gid,
            groups.as_mut_ptr(),
            &mut count,
        )
    } < 0
    {
        if !(1..=65536).contains(&count) {
            return Err(invalid("invalid supplementary group count"));
        }
        groups.resize(count as usize, 0);
        if unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                record.pw_gid,
                groups.as_mut_ptr(),
                &mut count,
            )
        } < 0
        {
            return Err(invalid("could not resolve supplementary groups"));
        }
    }
    groups.truncate(count as usize);
    Ok(Identity {
        uid: record.pw_uid,
        gid: record.pw_gid,
        groups,
    })
}

pub fn current_username() -> Result<String> {
    super::accounts::name(
        unsafe { libc::geteuid() },
        65536,
        "effective user has no account",
        "account name is not UTF-8",
    )
}

/// Legacy process-wide inheritance. The caller must exclude concurrent forks
/// from this call until exec (or flag restoration). Prefer `exec` for handover
/// and `CommandSpec::leases` for worker-local inheritance.
pub fn inherit_fd(fd: RawFd) -> Result<()> {
    // SAFETY: fcntl only examines/updates descriptor flags; invalid descriptors fail.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: descriptor flags are updated synchronously before any polling reads.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn text(bytes: &[u8]) -> Result<String> {
    Ok(String::from_utf8(bytes.to_vec())
        .map_err(|_| invalid("worker output is not UTF-8"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

fn pump(reader: &mut impl Read, output: Option<&mut Vec<u8>>, maximum: usize) -> Result<bool> {
    let mut output = output;
    let mut bytes = [0; 65536];
    // Bound work per poll so a noisy stderr cannot starve the execution deadline.
    for _ in 0..16 {
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if let Some(value) = output.as_deref_mut() {
                    if value.len().saturating_add(count) > maximum {
                        return Err(invalid("storage worker receipt exceeded its size limit"));
                    }
                    value.extend_from_slice(&bytes[..count]);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

pub fn run(spec: &CommandSpec) -> Result<Output> {
    let program = spec
        .argv
        .first()
        .ok_or_else(|| invalid("empty worker command"))?;
    if spec.timeout.is_zero() || spec.maximum_output == 0 {
        return Err(invalid("invalid worker limits"));
    }
    let mut command = Command::new(program);
    command
        .args(&spec.argv[1..])
        .stdin(if spec.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(environment) = &spec.environment {
        command.env_clear().envs(environment);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let leases = spec.leases.clone();
    let identity = spec.identity.clone();
    // SAFETY: the child hook uses only async-signal-safe syscalls and preallocated data.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in &leases {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(identity) = &identity
                && (libc::setgroups(identity.groups.len(), identity.groups.as_ptr()) != 0
                    || libc::setgid(identity.gid) != 0
                    || libc::setuid(identity.uid) != 0)
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = spawn(&mut command)?;
    let pid = child.id() as i32;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing worker stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| invalid("missing worker stderr"))?;
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let mut stdin = child.stdin.take();
        if let Some(input) = &stdin {
            nonblocking(input.as_raw_fd())?;
        }
        let mut written = 0;
        let mut output = Vec::new();
        let mut stdout_done = false;
        let mut stderr_done = false;
        let start = Instant::now();
        loop {
            if start.elapsed() >= spec.timeout {
                return Err(invalid("storage worker exceeded its execution limit"));
            }
            if let (Some(input), Some(pipe)) = (&spec.input, &mut stdin) {
                match pipe.write(&input[written..]) {
                    Ok(count) => {
                        written += count;
                        if written == input.len() {
                            stdin = None;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if !stdout_done {
                stdout_done = pump(&mut stdout, Some(&mut output), spec.maximum_output)?;
            }
            if !stderr_done {
                stderr_done = pump(&mut stderr, None, 0)?;
            }
            // Leave an exited leader unreaped until its pipes close. Its PID then
            // cannot be recycled while cancellation targets its process group.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: info is valid writable storage; WNOWAIT retains child ownership.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if unsafe { info.si_pid() } != 0 && stdout_done && stderr_done {
                return Ok(Output {
                    status: child.wait()?,
                    stdout: output,
                    stderr: Vec::new(),
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        // The leader is still owned and unreaped; the process-group ID cannot
        // have been recycled. All descendants retain leases until they exit.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    result
}

pub fn execute(spec: &CommandSpec) -> Result<Vec<u8>> {
    let output = run(spec)?;
    if !output.status.success() {
        return Err(invalid("storage worker failed; diagnostics suppressed"));
    }
    Ok(output.stdout)
}
