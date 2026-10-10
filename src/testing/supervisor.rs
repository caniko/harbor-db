//! Durable worker/observer/follower services.
//!
//! CLI contract: `worker DIR` calls [`worker`], `observe DIR` calls [`observe`].
//! `create_run` returns the absolute run directory; all service functions return
//! an error for malformed storage or busy leases. Execution failures are receipts,
//! not service errors. `verify` returns a verdict and reasons; `watch` is read-only.
use super::{
    evidence::{self, ArtifactReceipt, ArtifactSpec},
    host,
};
use crate::storage::durable;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            fs::DirBuilderExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub(crate) fn error(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileBinding {
    pub path: PathBuf,
    pub sha256: String,
}
pub fn bind_file(path: &Path) -> Result<FileBinding> {
    Ok(FileBinding {
        path: path.to_path_buf(),
        sha256: evidence::hash(&evidence::bounded_read(path)?),
    })
}

/// Complete regular-file inventory of an immutable retained source snapshot.
/// Symlinks and special files are rejected; callers may append external inputs.
pub fn bind_tree(root: &Path) -> Result<Vec<FileBinding>> {
    let directory = evidence::open(root, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let mut bindings = Vec::new();
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        let entry = entry?;
        let path = root.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            bindings.extend(bind_tree(&path)?);
        } else if kind.is_file() {
            bindings.push(bind_file(&path)?);
        } else {
            return Err(error("source inventory contains symlink or special file"));
        }
    }
    bindings.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(bindings)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Execution {
    Argv {
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Nix {
        installable: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseSpec {
    pub id: String,
    pub execution: Execution,
    pub deadline_seconds: u64,
    pub artifacts: Vec<ArtifactSpec>,
    pub dependencies: Vec<String>,
    pub resources: Vec<String>,
    pub platform: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub schema: u32,
    pub source_root: PathBuf,
    pub inputs: Vec<FileBinding>,
    pub cases: Vec<CaseSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Exited,
    Signalled,
    Timeout,
    Cancelled,
    Interrupted,
    SpawnFailed,
    DependencyFailed,
    UnsupportedPlatform,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub id: String,
    pub reason: ExitReason,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub started: u64,
    pub finished: u64,
    pub artifacts: Vec<ArtifactReceipt>,
    pub evidence_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
    Incomplete,
    Indeterminate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verification {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRegistration {
    pub boot_id: String,
    pub pid: u32,
    pub start_time: String,
    pub cgroup: String,
    pub systemd_invocation: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Started {
    pub worker: ProcessRegistration,
    pub child: Option<ProcessRegistration>,
    pub case_id: Option<String>,
    pub unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: String,
    pub started: Option<Started>,
    pub terminal: bool,
    pub worker_alive: bool,
    pub results: Vec<CaseResult>,
    pub verification: Verification,
}

fn publish<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    evidence::atomic_write(path, &bytes)
}
fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    evidence::check_path(path)?;
    Ok(serde_json::from_slice(&evidence::bounded_read(path)?)?)
}
fn optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read(path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(error("storage directory must be absolute"));
    }
    let mut prefix = PathBuf::from("/");
    for component in path
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
    {
        let std::path::Component::Normal(name) = component else {
            return Err(error("storage traversal forbidden"));
        };
        let directory = evidence::open(&prefix, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        use std::os::unix::ffi::OsStrExt;
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(std::io::Error::last_os_error().into());
        }
        prefix.push(name);
        evidence::open(&prefix, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok(())
}
fn log_file(path: &Path) -> Result<File> {
    let file = evidence::open(path, libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT, 0o600)?;
    if !file.metadata()?.is_file() {
        return Err(error("log is not a regular file"));
    }
    Ok(file)
}
fn lease(path: &Path, create: bool) -> Result<durable::Lease> {
    let parent = evidence::open(
        path.parent().ok_or_else(|| error("lease parent missing"))?,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    Ok(durable::lock(
        &PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(
            path.file_name()
                .ok_or_else(|| error("lease filename missing"))?,
        ),
        false,
        create,
    )?)
}

fn validate_spec(spec: &RunSpec) -> Result<()> {
    if spec.schema != 1 || spec.cases.is_empty() || spec.inputs.is_empty() {
        return Err(error("schema 1 requires cases and source/input bindings"));
    }
    evidence::check_path(&spec.source_root)?;
    if !spec.source_root.is_dir() {
        return Err(error("source root is missing"));
    }
    let mut seen = BTreeSet::new();
    let mut inputs = BTreeMap::new();
    for input in &spec.inputs {
        if inputs.insert(&input.path, &input.sha256).is_some()
            || bind_file(&input.path)?.sha256 != input.sha256
        {
            return Err(error("source/input binding mismatch or duplicate"));
        }
    }
    let inventory = bind_tree(&spec.source_root)?;
    if inventory.is_empty()
        || inventory
            .iter()
            .any(|b| inputs.get(&b.path) != Some(&&b.sha256))
        || spec
            .inputs
            .iter()
            .filter(|b| b.path.starts_with(&spec.source_root))
            .count()
            != inventory.len()
    {
        return Err(error("source corpus inventory is incomplete or changed"));
    }
    for case in &spec.cases {
        if !safe_id(&case.id) || !seen.insert(case.id.clone()) || case.deadline_seconds == 0 {
            return Err(error("invalid case identity or deadline"));
        }
        if case
            .dependencies
            .iter()
            .any(|id| !seen.contains(id) || id == &case.id)
        {
            return Err(error("dependencies must precede cases"));
        }
        if case.resources.iter().any(|id| !safe_id(id)) {
            return Err(error("invalid resource identity"));
        }
        // Receipts must not retain secret values. Only explicit non-secret env belongs in a RunSpec.
        if let Execution::Argv { argv, env } = &case.execution
            && (argv.is_empty()
                || argv[0].is_empty()
                || argv.iter().any(|v| v.contains('\0'))
                || env
                    .iter()
                    .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0')))
        {
            return Err(error("invalid argv or environment"));
        }
        for artifact in &case.artifacts {
            if artifact.source != case.id || !artifact.path.is_absolute() {
                return Err(error("artifact must bind its case and absolute path"));
            }
        }
    }
    Ok(())
}

/// Creates a new private run under an explicit base or XDG state. Never reuses IDs.
pub fn create_run(base: Option<&Path>, id: &str, spec: RunSpec) -> Result<PathBuf> {
    if !safe_id(id) {
        return Err(error("invalid run ID"));
    }
    validate_spec(&spec)?;
    let base = match base {
        Some(path) => path.to_path_buf(),
        None => {
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .ok_or_else(|| error("state home unavailable"))?;
            state.join("harbor-db/tests/runs")
        }
    };
    private_dir(&base)?;
    let base = base.canonicalize()?;
    let _creation = lease(&base.join("create.lock"), true)?;
    let run = base.join(id);
    fs::DirBuilder::new().mode(0o700).create(&run)?;
    let _worker = lease(&run.join("worker.lock"), true)?;
    let _observer = lease(&run.join("observer.lock"), true)?;
    publish(&run.join("spec.json"), &spec)?;
    publish(
        &run.join("created.json"),
        &serde_json::json!({"unix_seconds": now()}),
    )?;
    evidence::atomic_write(
        &run.join("spec.sha256"),
        evidence::hash(&evidence::bounded_read(&run.join("spec.json"))?).as_bytes(),
    )?;
    durable::sync_directory(&base)?;
    Ok(run)
}

fn read_spec(run: &Path) -> Result<RunSpec> {
    evidence::check_path(run)?;
    let bytes = evidence::bounded_read(&run.join("spec.json"))?;
    let expected = evidence::bounded_read(&run.join("spec.sha256"))?;
    if evidence::hash(&bytes).as_bytes() != expected {
        return Err(error("run specification integrity mismatch"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn load_spec(run: &Path) -> Result<RunSpec> {
    let spec = read_spec(run)?;
    validate_spec(&spec)?;
    validate_service_executable(run)?;
    Ok(spec)
}

fn validate_service_executable(run: &Path) -> Result<()> {
    let binding = optional::<FileBinding>(&run.join("service-executable.json"))?;
    if binding.is_none() && run.join("launch-requested.json").exists() {
        return Err(error("detached service executable binding missing"));
    }
    if let Some(binding) = binding
        && (binding.path != run.join("service-executable")
            || bind_file(&binding.path)?.sha256 != binding.sha256)
    {
        return Err(error("retained service executable binding changed"));
    }
    Ok(())
}

fn artifacts(run: &Path, case: &CaseSpec) -> Vec<ArtifactSpec> {
    let mut artifacts = case.artifacts.clone();
    if matches!(case.execution, Execution::Nix { .. })
        && !artifacts
            .iter()
            .any(|a| matches!(a.kind, evidence::ArtifactKind::NixOutputs) && a.required)
    {
        artifacts.push(ArtifactSpec {
            source: case.id.clone(),
            path: run.join(format!("{}.nix-outputs.json", case.id)),
            kind: evidence::ArtifactKind::NixOutputs,
            required: true,
            sha256: None,
        });
    }
    artifacts
}

fn nix_provenance(run: &Path, case: &CaseSpec) -> Result<()> {
    let builds: serde_json::Value = serde_json::from_slice(&evidence::bounded_read(
        &run.join(format!("{}.stdout.log", case.id)),
    )?)?;
    let builds = builds
        .as_array()
        .ok_or_else(|| error("Nix build JSON is not an array"))?;
    let mut outputs = Vec::new();
    for build in builds {
        let _ = publish(
            &run.join("heartbeat.json"),
            &serde_json::json!({"unix_seconds": now(), "case_id": case.id, "phase": "nix_provenance"}),
        );
        let paths = build
            .get("outputs")
            .and_then(|v| v.as_object())
            .ok_or_else(|| error("Nix build outputs missing"))?;
        let derivation = PathBuf::from(
            build
                .get("drvPath")
                .and_then(|value| value.as_str())
                .ok_or_else(|| error("Nix derivation provenance missing"))?,
        );
        if derivation.parent() != Some(Path::new("/nix/store"))
            || derivation
                .extension()
                .is_none_or(|extension| extension != "drv")
        {
            return Err(error("invalid Nix derivation provenance"));
        }
        // A rooted output alone need not retain the complete source/input graph.
        // A bare derivation installable retains the derivation itself on the
        // supported Nix API; named outputs use the explicit ^out selector.
        let derivation_root = run.join(format!(
            "{}.nix-derivation-root-{}",
            case.id,
            &evidence::hash(derivation.as_os_str().as_encoded_bytes())[..16]
        ));
        evidence::bounded_diagnostic(
            vec![
                "nix".into(),
                "build".into(),
                "--max-jobs".into(),
                "0".into(),
                "--option".into(),
                "builders".into(),
                String::new(),
                "--option".into(),
                "post-build-hook".into(),
                String::new(),
                "--no-update-lock-file".into(),
                "--out-link".into(),
                derivation_root.to_string_lossy().into_owned(),
                "--json".into(),
                "--".into(),
                derivation.to_string_lossy().into_owned(),
            ],
            Duration::from_secs(15),
        )?;
        if fs::read_link(&derivation_root)? != derivation {
            return Err(error(
                "Nix derivation GC root does not retain its source/input graph",
            ));
        }
        let record = evidence::nix_path_info(&derivation)?;
        outputs.push(evidence::NixOutput {
            store_path: derivation,
            gc_root: derivation_root,
            nar_hash: record
                .get("narHash")
                .and_then(|value| value.as_str())
                .ok_or_else(|| error("Nix derivation NAR hash missing"))?
                .to_owned(),
        });
        for value in paths.values() {
            let store_path = PathBuf::from(
                value
                    .as_str()
                    .ok_or_else(|| error("Nix output path missing"))?,
            );
            let mut gc_root = None;
            for entry in fs::read_dir(run)? {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{}.nix-root", case.id))
                    && fs::read_link(entry.path()).is_ok_and(|p| p == store_path)
                {
                    gc_root = Some(entry.path());
                    break;
                }
            }
            let gc_root = gc_root.ok_or_else(|| error("Nix output lacks retained GC root"))?;
            let record = evidence::nix_path_info(&store_path)?;
            let nar_hash = record
                .get("narHash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error("Nix NAR hash missing"))?
                .to_owned();
            outputs.push(evidence::NixOutput {
                store_path,
                gc_root,
                nar_hash,
            });
        }
    }
    for artifact in artifacts(run, case)
        .iter()
        .filter(|a| matches!(a.kind, evidence::ArtifactKind::NixOutputs))
    {
        let parent = artifact
            .path
            .parent()
            .ok_or_else(|| error("Nix artifact parent missing"))?;
        private_dir(parent)?;
        publish(&artifact.path, &outputs)?;
    }
    for artifact in artifacts(run, case).iter().filter(|artifact| {
        matches!(
            artifact.kind,
            evidence::ArtifactKind::Junit | evidence::ArtifactKind::Semantic
        )
    }) {
        private_dir(
            artifact
                .path
                .parent()
                .ok_or_else(|| error("Nix acceptance artifact parent missing"))?,
        )?;
        evidence::import_nix_document(
            artifact,
            &outputs
                .iter()
                .map(|output| output.store_path.clone())
                .collect::<Vec<_>>(),
        )?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct RegistrationProbe {
    current: Option<ProcessRegistration>,
    error: Option<ProbeError>,
    raw_stat: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProbeError {
    stage: &'static str,
    message: String,
    raw_os_error: Option<i32>,
}

fn probe_registration(pid: u32) -> RegistrationProbe {
    let mut probe = RegistrationProbe {
        current: None,
        error: None,
        raw_stat: None,
    };
    let mut stage = "stat read";
    let result = (|| -> Result<ProcessRegistration> {
        // Bind the process's main task (thread-group leader) explicitly. User-mode
        // emulation can synthesize /proc/<pid>/stat from the calling thread,
        // giving different start times to a worker and its observer. The leader
        // task path keeps the same PID/start identity across both callers.
        let stat = fs::read_to_string(format!("/proc/{pid}/task/{pid}/stat"))?;
        // Retain at most 4096 characters, even if procfs is emulated incorrectly.
        probe.raw_stat = Some(stat.chars().take(4096).collect());
        stage = "stat parse";
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or_else(|| error("invalid process stat"))?
            .1
            .split_whitespace()
            .collect();
        stage = "stat starttime";
        let start_time = fields
            .get(19)
            .ok_or_else(|| error("missing process start time"))?
            .to_string();
        stage = "boot ID read";
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into();
        stage = "cgroup read";
        let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        Ok(ProcessRegistration {
            boot_id,
            pid,
            start_time,
            cgroup,
            systemd_invocation: std::env::var("INVOCATION_ID").ok(),
        })
    })();
    match result {
        Ok(current) => probe.current = Some(current),
        Err(e) => {
            probe.error = Some(ProbeError {
                stage,
                message: e.to_string(),
                raw_os_error: e
                    .downcast_ref::<std::io::Error>()
                    .and_then(|e| e.raw_os_error()),
            })
        }
    }
    probe
}

fn registration(pid: u32) -> Result<ProcessRegistration> {
    let probe = probe_registration(pid);
    probe.current.ok_or_else(|| {
        error(match probe.error {
            Some(failure) => format!("{}: {}", failure.stage, failure.message),
            None => "registration probe has no result".into(),
        })
    })
}

#[derive(Debug, Serialize)]
struct WorkerLivenessProbe {
    unix_seconds: u64,
    caller_pid: u32,
    caller_thread_id: Option<i64>,
    saved: ProcessRegistration,
    observed: RegistrationProbe,
    alive: bool,
}

fn probe_worker(saved: &ProcessRegistration) -> WorkerLivenessProbe {
    let observed = probe_registration(saved.pid);
    let alive = observed.current.as_ref().is_some_and(|current| {
        current.boot_id == saved.boot_id && current.start_time == saved.start_time
    });
    #[cfg(target_os = "linux")]
    // SAFETY: gettid has no pointer arguments and only queries the calling thread.
    let caller_thread_id = Some(unsafe { libc::syscall(libc::SYS_gettid) } as i64);
    #[cfg(not(target_os = "linux"))]
    let caller_thread_id = None;
    WorkerLivenessProbe {
        unix_seconds: now(),
        caller_pid: std::process::id(),
        caller_thread_id,
        saved: saved.clone(),
        observed,
        alive,
    }
}

/// Poll without reaping: the owned group leader must pin its PID until cleanup.
fn child_exited(child: &Child) -> Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: info is writable; this queries only our owned child and never reaps it.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { info.si_pid() } != 0)
}
fn kill_owned_group(child: &Child) -> Result<()> {
    // SAFETY: child is unreaped and setsid made this exact PID the private group
    // leader. Its PID cannot be reused before wait. Never called on saved PIDs.
    if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e.into());
        }
    }
    Ok(())
}

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl HeartbeatGuard {
    fn start(run: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let run = run.to_path_buf();
        let thread = thread::spawn(move || {
            loop {
                for _ in 0..100 {
                    if flag.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let current: Option<Started> = optional(&run.join("started.json")).ok().flatten();
                let receipt = serde_json::json!({"unix_seconds": now(), "case_id": current.and_then(|s| s.case_id)});
                if let Err(e) = publish(&run.join("heartbeat.json"), &receipt) {
                    let _ = publish(
                        &run.join("heartbeat-error.json"),
                        &serde_json::json!({"unix_seconds": now(), "reason": e.to_string()}),
                    );
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn execute_case(
    run: &Path,
    spec: &RunSpec,
    case: &CaseSpec,
    authority: &durable::Lease,
    resources: &[durable::Lease],
    started: &mut Started,
) -> Result<CaseResult> {
    let mut result = CaseResult {
        id: case.id.clone(),
        reason: ExitReason::Exited,
        code: None,
        signal: None,
        started: now(),
        finished: now(),
        artifacts: vec![],
        evidence_errors: vec![],
    };
    let mut command = match &case.execution {
        Execution::Argv { argv, env } => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).env_clear().envs(env);
            command
        }
        Execution::Nix { installable } => {
            let mut command = Command::new("nix");
            command
                .args([
                    "build",
                    "--print-build-logs",
                    "--pure-eval",
                    "--no-allow-import-from-derivation",
                    "--no-update-lock-file",
                    "--option",
                    "post-build-hook",
                    "",
                    "--max-jobs",
                    "1",
                    "--cores",
                    "2",
                    "--json",
                    "--out-link",
                ])
                .arg(run.join(format!("{}.nix-root", case.id)))
                .arg(installable);
            command
        }
    };
    command
        .current_dir(&spec.source_root)
        .stdin(Stdio::null())
        .stdout(log_file(&run.join(format!("{}.stdout.log", case.id)))?)
        .stderr(log_file(&run.join(format!("{}.stderr.log", case.id)))?);
    let lease_fds: Vec<_> = std::iter::once(authority.fd())
        .chain(resources.iter().map(|l| l.fd()))
        .collect();
    // SAFETY: only async-signal-safe libc operations run between fork and exec.
    // The child inherits authority so worker loss cannot silently release it.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in &lease_fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = match crate::storage::process::spawn(&mut command) {
        Ok(child) => child,
        Err(e) => {
            result.reason = ExitReason::SpawnFailed;
            result.evidence_errors.push(e.to_string());
            return Ok(result);
        }
    };
    started.child = registration(child.id()).ok();
    started.case_id = Some(case.id.clone());
    if let Err(e) = publish(&run.join("started.json"), started) {
        let _ = kill_owned_group(&child);
        let _ = child.wait();
        return Err(e);
    }
    let began = Instant::now();
    let mut heartbeat = Instant::now();
    let _ = publish(
        &run.join("heartbeat.json"),
        &serde_json::json!({"unix_seconds": now(), "case_id": case.id, "elapsed_seconds": 0}),
    );
    let polling = (|| -> Result<()> {
        loop {
            if child_exited(&child)? {
                break;
            }
            if run.join("cancel.json").exists() {
                result.reason = ExitReason::Cancelled;
                break;
            }
            if began.elapsed() >= Duration::from_secs(case.deadline_seconds) {
                result.reason = ExitReason::Timeout;
                break;
            }
            if heartbeat.elapsed() >= Duration::from_secs(10) {
                let _ = publish(
                    &run.join("heartbeat.json"),
                    &serde_json::json!({"unix_seconds": now(), "case_id": case.id, "elapsed_seconds": began.elapsed().as_secs()}),
                );
                heartbeat = Instant::now();
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();
    // Clean remaining descendants while the unreaped leader still pins the PGID.
    let cleanup = kill_owned_group(&child);
    let exit = child.wait();
    polling?;
    cleanup?;
    let exit = exit?;
    result.code = exit.code();
    result.signal = exit.signal();
    if result.reason == ExitReason::Exited && result.signal.is_some() {
        result.reason = ExitReason::Signalled;
    }
    result.finished = now();
    started.child = None;
    Ok(result)
}

/// Executes exactly once under a persistent authority anchor; never restarts work.
pub fn worker(run: &Path) -> Result<()> {
    evidence::check_path(run)?;
    let authority = lease(&run.join("worker.lock"), false)?;
    if run.join("started.json").exists() || run.join("launch-failed.json").exists() {
        return Err(error(
            "run already started or launch failed; observer may attach but worker never reruns",
        ));
    }
    let spec = load_spec(run)?;
    let mut started = Started {
        worker: registration(std::process::id())?,
        child: None,
        case_id: None,
        unix_seconds: now(),
    };
    publish(&run.join("started.json"), &started)?;
    let _heartbeat = HeartbeatGuard::start(run);
    let mut results: Vec<CaseResult> = Vec::new();
    let mut artifact_index = 0;
    for case in &spec.cases {
        let mut skipped = None;
        if run.join("cancel.json").exists() {
            skipped = Some(ExitReason::Cancelled);
        } else if !matches!(case.platform.as_str(), "linux" | "any")
            && case.platform != format!("{}-linux", std::env::consts::ARCH)
        {
            skipped = Some(ExitReason::UnsupportedPlatform);
        } else if case.dependencies.iter().any(|id| {
            !results.iter().any(|r| {
                &r.id == id
                    && r.reason == ExitReason::Exited
                    && r.code == Some(0)
                    && r.evidence_errors.is_empty()
            })
        }) {
            skipped = Some(ExitReason::DependencyFailed);
        }
        let mut resources = case.resources.clone();
        resources.sort();
        resources.dedup();
        let resources_dir = run
            .parent()
            .ok_or_else(|| error("run base missing"))?
            .join("resources");
        private_dir(&resources_dir)?;
        let mut leases = Vec::new();
        for resource in &resources {
            leases.push(lease(
                &resources_dir.join(format!("{resource}.lock")),
                true,
            )?);
        }
        let descriptors = artifacts(run, case);
        let before: Vec<_> = descriptors
            .iter()
            .map(|a| artifact_identity(&a.path).ok())
            .collect();
        let mut result = if let Some(reason) = skipped {
            CaseResult {
                id: case.id.clone(),
                reason,
                code: None,
                signal: None,
                started: now(),
                finished: now(),
                artifacts: vec![],
                evidence_errors: vec![],
            }
        } else {
            execute_case(run, &spec, case, &authority, &leases, &mut started)?
        };
        if matches!(case.execution, Execution::Nix { .. })
            && result.reason == ExitReason::Exited
            && result.code == Some(0)
            && let Err(e) = nix_provenance(run, case)
        {
            result.evidence_errors.push(e.to_string());
        }
        for (artifact, old) in descriptors.iter().zip(before) {
            if old.is_some() && artifact_identity(&artifact.path).ok() == old {
                if artifact.required {
                    result.evidence_errors.push(format!(
                        "{}: artifact was not refreshed by this execution",
                        artifact.path.display()
                    ));
                }
                artifact_index += 1;
                continue;
            }
            match evidence::capture(run, artifact_index, artifact) {
                Ok(receipt) => result.artifacts.push(receipt),
                Err(e) if artifact.required => result.evidence_errors.push(e.to_string()),
                Err(_) => {}
            }
            artifact_index += 1;
        }
        results.push(result);
        publish(&run.join("results.json"), &results)?;
        let _ = publish(
            &run.join("progress.json"),
            &serde_json::json!({"unix_seconds": now(), "completed_cases": results.len(), "case_id": case.id}),
        );
    }
    publish(
        &run.join("terminal.json"),
        &serde_json::json!({"finished": now(), "results_sha256": evidence::hash(&evidence::bounded_read(&run.join("results.json"))?)}),
    )?;
    Ok(())
}
fn artifact_identity(path: &Path) -> Result<(u64, u64, i64, i64, i64, i64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = evidence::open(path, libc::O_RDONLY, 0)?.metadata()?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.len(),
    ))
}

/// Validates retained input, specification, terminal receipt and acceptance files.
pub fn verify(run: &Path) -> Result<Verification> {
    evidence::check_path(run)?;
    let spec = match read_spec(run) {
        Ok(spec) => spec,
        Err(e) => {
            return Ok(Verification {
                verdict: Verdict::Indeterminate,
                reasons: vec![e.to_string()],
            });
        }
    };
    let mut reasons = Vec::new();
    let mut verdict = Verdict::Passed;
    let mut raise = |next: Verdict| {
        if rank(&next) > rank(&verdict) {
            verdict = next;
        }
    };
    if let Some(failure) = optional::<serde_json::Value>(&run.join("launch-failed.json"))? {
        raise(Verdict::Failed);
        reasons.push(format!(
            "detached launch failed: {}",
            failure
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown launch error")
        ));
        return Ok(Verification { verdict, reasons });
    }
    if let Err(e) = validate_spec(&spec) {
        raise(Verdict::Indeterminate);
        reasons.push(e.to_string());
    }
    if let Err(e) = validate_service_executable(run) {
        raise(Verdict::Indeterminate);
        reasons.push(e.to_string());
    }
    let interrupted =
        !run.join("terminal.json").exists() && run.join("interrupted-terminal.json").exists();
    let result_path = run.join(if interrupted {
        "interrupted.json"
    } else {
        "results.json"
    });
    let terminal_path = run.join(if interrupted {
        "interrupted-terminal.json"
    } else {
        "terminal.json"
    });
    let results: Vec<CaseResult> = match optional(&result_path) {
        Ok(Some(results)) => results,
        Ok(None) => {
            raise(Verdict::Incomplete);
            reasons.push("execution results missing".into());
            return Ok(Verification { verdict, reasons });
        }
        Err(e) => {
            raise(Verdict::Indeterminate);
            reasons.push(e.to_string());
            return Ok(Verification { verdict, reasons });
        }
    };
    let terminal: Option<serde_json::Value> = match optional(&terminal_path) {
        Ok(value) => value,
        Err(e) => {
            raise(Verdict::Indeterminate);
            reasons.push(e.to_string());
            None
        }
    };
    if let Some(terminal) = &terminal {
        if terminal.get("results_sha256").and_then(|v| v.as_str())
            != Some(evidence::hash(&evidence::bounded_read(&result_path)?).as_str())
            || results.len() != spec.cases.len()
        {
            raise(Verdict::Indeterminate);
            reasons.push("terminal result integrity mismatch".into());
            return Ok(Verification { verdict, reasons });
        }
        if interrupted {
            let original_hash = optional::<Vec<CaseResult>>(&run.join("results.json"))?
                .map(|_| {
                    evidence::bounded_read(&run.join("results.json")).map(|b| evidence::hash(&b))
                })
                .transpose()?;
            if terminal
                .get("original_results_sha256")
                .and_then(|v| v.as_str())
                != original_hash.as_deref()
            {
                raise(Verdict::Indeterminate);
                reasons.push("pre-interruption results integrity mismatch".into());
                return Ok(Verification { verdict, reasons });
            }
        }
    } else {
        raise(Verdict::Incomplete);
        reasons.push("worker has not published terminal evidence".into());
    }
    for (case, result) in spec.cases.iter().zip(&results) {
        if result.id != case.id {
            raise(Verdict::Indeterminate);
            reasons.push("case identity mismatch".into());
            continue;
        }
        if result.reason != ExitReason::Exited
            || result.code != Some(0)
            || result.signal.is_some()
            || !result.evidence_errors.is_empty()
        {
            raise(
                if matches!(
                    result.reason,
                    ExitReason::Interrupted | ExitReason::UnsupportedPlatform
                ) {
                    Verdict::Incomplete
                } else {
                    Verdict::Failed
                },
            );
            reasons.push(format!(
                "{}: {:?}, code {:?}, signal {:?}",
                case.id, result.reason, result.code, result.signal
            ));
            reasons.extend(result.evidence_errors.clone());
        }
        if matches!(
            result.reason,
            ExitReason::Interrupted | ExitReason::UnsupportedPlatform
        ) {
            continue;
        }
        let descriptors = artifacts(run, case);
        if !descriptors
            .iter()
            .any(|a| a.required && !matches!(a.kind, evidence::ArtifactKind::File))
        {
            raise(Verdict::Failed);
            reasons.push(format!(
                "{}: acceptance requires semantic, JUnit or Nix output evidence",
                case.id
            ));
        }
        for artifact in &descriptors {
            match result
                .artifacts
                .iter()
                .find(|r| r.source == artifact.source && r.original_path == artifact.path)
            {
                Some(receipt) => {
                    if let Err(e) = evidence::validate(run, artifact, receipt) {
                        raise(Verdict::Indeterminate);
                        reasons.push(e.to_string());
                    }
                }
                None if artifact.required => {
                    raise(Verdict::Failed);
                    reasons.push(format!("{}: required artifact missing", case.id));
                }
                None => {}
            }
        }
    }
    Ok(Verification { verdict, reasons })
}
fn rank(verdict: &Verdict) -> u8 {
    match verdict {
        Verdict::Passed => 0,
        Verdict::Incomplete => 1,
        Verdict::Indeterminate => 2,
        Verdict::Failed => 3,
    }
}

pub fn status(run: &Path) -> Result<RunStatus> {
    status_with_probe(run).map(|(state, _)| state)
}

fn status_with_probe(run: &Path) -> Result<(RunStatus, Option<WorkerLivenessProbe>)> {
    evidence::check_path(run)?;
    let started: Option<Started> = optional(&run.join("started.json"))?;
    let probe = started.as_ref().map(|s| probe_worker(&s.worker));
    let interrupted =
        !run.join("terminal.json").exists() && run.join("interrupted-terminal.json").exists();
    Ok((
        RunStatus {
            run_id: run
                .file_name()
                .ok_or_else(|| error("missing run ID"))?
                .to_string_lossy()
                .into(),
            worker_alive: probe.as_ref().is_some_and(|p| p.alive),
            started,
            terminal: run.join("terminal.json").exists()
                || interrupted
                || run.join("launch-failed.json").exists(),
            results: optional(&run.join(if interrupted {
                "interrupted.json"
            } else {
                "results.json"
            }))?
            .unwrap_or_default(),
            verification: verify(run)?,
        },
        probe,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Activity {
    pub unix_seconds: u64,
    pub log_bytes: u64,
    pub last_log_seconds: u64,
    pub log_age_seconds: u64,
    pub semantic_progress_seconds: u64,
    pub semantic_progress_age_seconds: u64,
    pub heartbeat_age_seconds: Option<u64>,
    pub quiet: bool,
    /// Fresh output and a live heartbeat do not establish meaningful progress.
    #[serde(default)]
    pub stalled: bool,
}

fn modified_seconds(path: &Path) -> Option<u64> {
    evidence::open(path, libc::O_RDONLY, 0)
        .ok()?
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn activity(run: &Path, state: &RunStatus) -> Result<Activity> {
    let time = now();
    let created = optional::<serde_json::Value>(&run.join("created.json"))?
        .and_then(|v| v.get("unix_seconds").and_then(|v| v.as_u64()))
        .unwrap_or(time);
    let baseline = state.started.as_ref().map_or(created, |s| s.unix_seconds);
    let mut last_log = baseline;
    let mut bytes = 0;
    for entry in fs::read_dir(run)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().ends_with(".log") {
            continue;
        }
        let file = evidence::open(&entry.path(), libc::O_RDONLY, 0)?;
        let metadata = file.metadata()?;
        bytes += metadata.len();
        if metadata.len() != 0 {
            last_log = last_log.max(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_secs());
        }
    }
    let progress = optional::<serde_json::Value>(&run.join("progress.json"))?
        .and_then(|v| v.get("unix_seconds").and_then(|v| v.as_u64()))
        .unwrap_or(baseline)
        .max(modified_seconds(&run.join("started.json")).unwrap_or(baseline));
    let heartbeat = optional::<serde_json::Value>(&run.join("heartbeat.json"))?
        .and_then(|v| v.get("unix_seconds").and_then(|v| v.as_u64()));
    Ok(Activity {
        unix_seconds: time,
        log_bytes: bytes,
        last_log_seconds: last_log,
        log_age_seconds: time.saturating_sub(last_log),
        semantic_progress_seconds: progress,
        semantic_progress_age_seconds: time.saturating_sub(progress),
        heartbeat_age_seconds: heartbeat.map(|t| time.saturating_sub(t)),
        quiet: state.started.is_some() && !state.terminal && time.saturating_sub(last_log) >= 300,
        stalled: state.started.is_some() && !state.terminal && time.saturating_sub(progress) >= 300,
    })
}

fn journal_event(run: &Path, kind: &str, value: serde_json::Value) -> Result<()> {
    let event =
        serde_json::json!({"event": kind, "run": run, "unix_seconds": now(), "details": value});
    // Broken follower/terminal output must never change execution or acceptance.
    let _ = writeln!(std::io::stdout().lock(), "{event}");
    publish(&run.join(format!("{kind}-notification.json")), &event)
}

fn notify_terminal(run: &Path, state: &RunStatus) -> Result<()> {
    if run.join("terminal-notification.json").exists() {
        return Ok(());
    }
    journal_event(run, "terminal", serde_json::to_value(&state.verification)?)?;
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        let argv = vec![
            "notify-send".into(),
            "--app-name=HarborDB tests".into(),
            format!("HarborDB: {}", state.run_id),
            format!("{:?}", state.verification.verdict),
        ];
        if let Err(e) = evidence::bounded_diagnostic(argv, Duration::from_secs(2)) {
            let _ = publish(
                &run.join("desktop-notification-error.json"),
                &serde_json::json!({"reason": e.to_string(), "unix_seconds": now()}),
            );
        }
    }
    Ok(())
}

/// Cancellation is a durable request consumed only by the owning worker.
pub fn cancel(run: &Path) -> Result<()> {
    evidence::check_path(run)?;
    publish(
        &run.join("cancel.json"),
        &serde_json::json!({"requested": now()}),
    )
}

/// One attach/sample pass, useful for restarted observers and embedding.
fn sample_host(run: &Path) -> Result<()> {
    let _sample = lease(&run.join("host-sample.lock"), true)?;
    let log_path = run.join("host.jsonl");
    if fs::symlink_metadata(&log_path).is_ok_and(|m| m.is_file() && m.len() > 8 * 1024 * 1024) {
        fs::rename(&log_path, run.join("host.previous.jsonl"))?;
        durable::sync_directory(run)?;
    }
    let mut log = log_file(&log_path)?;
    let mut bytes = serde_json::to_vec(&host::sample())?;
    bytes.push(b'\n');
    log.write_all(&bytes)?;
    log.sync_data()?;
    Ok(())
}

pub fn observe_once(run: &Path) -> Result<RunStatus> {
    evidence::check_path(run)?;
    let _pass = lease(&run.join("observation.lock"), true)?;
    let (state, probe) = status_with_probe(run)?;
    if !state.terminal && state.started.as_ref().is_some_and(|_| !state.worker_alive) {
        // Persist the exact deciding probe, not a later retry. Diagnostic failure
        // must not alter the existing worker-loss/terminal policy or CLI stdout.
        let _ = publish(&run.join("worker-liveness.json"), &probe);
        // Only the observer's own receipt changes. Original worker results remain
        // byte-for-byte intact, and no saved child PID is ever signalled here.
        let spec = read_spec(run)?;
        let mut results = state.results.clone();
        let original_hash = optional::<Vec<CaseResult>>(&run.join("results.json"))?
            .map(|_| evidence::bounded_read(&run.join("results.json")).map(|b| evidence::hash(&b)))
            .transpose()?;
        for case in spec.cases.iter().skip(results.len()) {
            results.push(CaseResult {
                id: case.id.clone(),
                reason: ExitReason::Interrupted,
                code: None,
                signal: None,
                started: state.started.as_ref().map_or(now(), |s| s.unix_seconds),
                finished: now(),
                artifacts: vec![],
                evidence_errors: vec![],
            });
        }
        publish(&run.join("interrupted.json"), &results)?;
        publish(
            &run.join("interrupted-terminal.json"),
            &serde_json::json!({"finished": now(), "results_sha256": evidence::hash(&evidence::bounded_read(&run.join("interrupted.json"))?), "original_results_sha256": original_hash}),
        )?;
    }
    let state = status(run)?;
    if let Err(e) = sample_host(run) {
        let _ = publish(
            &run.join("host-sample-error.json"),
            &serde_json::json!({"reason": e.to_string(), "unix_seconds": now()}),
        );
    }
    publish(&run.join("observation.json"), &state)?;
    let activity = activity(run, &state)?;
    publish(&run.join("activity.json"), &activity)?;
    if activity.quiet {
        let notified = optional::<serde_json::Value>(&run.join("quiet-notification.json"))?
            .and_then(|v| {
                v.get("details")
                    .and_then(|v| v.get("last_log_seconds"))
                    .and_then(|v| v.as_u64())
            });
        if notified != Some(activity.last_log_seconds) {
            journal_event(run, "quiet", serde_json::to_value(&activity)?)?;
        }
    }
    if activity.stalled {
        let notified = optional::<serde_json::Value>(&run.join("stalled-notification.json"))?
            .and_then(|v| {
                v.get("details")
                    .and_then(|v| v.get("semantic_progress_seconds"))
                    .and_then(|v| v.as_u64())
            });
        if notified != Some(activity.semantic_progress_seconds) {
            journal_event(run, "stalled", serde_json::to_value(&activity)?)?;
        }
    }
    if state.terminal {
        notify_terminal(run, &state)?;
    }
    Ok(state)
}

/// Independent service: samples every 10 seconds, summarizes every 30, reports
/// quiet output and stalled meaningful progress after 300 independently. A
/// progress notification never terminates execution before its hard deadline.
pub fn observe(run: &Path) -> Result<()> {
    observe_service(run, &AtomicBool::new(false))
}
fn observe_service(run: &Path, stop: &AtomicBool) -> Result<()> {
    evidence::check_path(run)?;
    let _lease = lease(&run.join("observer.lock"), false)?;
    // Sampling has its own cadence even if terminal evidence requires slow Nix
    // diagnostics. Neither stdout activity nor semantic progress drives this clock.
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler_flag = sampler_stop.clone();
    let sample_run = run.to_path_buf();
    let sampler = thread::spawn(move || {
        while !sampler_flag.load(Ordering::Acquire) {
            if let Err(e) = sample_host(&sample_run) {
                let _ = publish(
                    &sample_run.join("host-sample-error.json"),
                    &serde_json::json!({"reason": e.to_string(), "unix_seconds": now()}),
                );
            }
            for _ in 0..100 {
                if sampler_flag.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    let mut summary = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            let state = observe_once(run)?;
            if state.terminal
                || stop.load(Ordering::Acquire)
                || state.started.as_ref().is_some_and(|_| !state.worker_alive)
            {
                return Ok(());
            }
            if summary.elapsed() >= Duration::from_secs(30) {
                publish(&run.join("summary.json"), &activity(run, &state)?)?;
                summary = Instant::now();
            }
            for _ in 0..100 {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    })();
    sampler_stop.store(true, Ordering::Release);
    let _ = sampler.join();
    result
}

/// Read-only follower. A time limit/disconnect drops only this function.
pub fn watch(run: &Path, interval: Duration, limit: Option<Duration>) -> Result<RunStatus> {
    let began = Instant::now();
    loop {
        let state = status(run)?;
        if state.terminal
            || limit.is_some_and(|limit| began.elapsed() >= limit)
            || state.started.as_ref().is_some_and(|_| !state.worker_alive)
        {
            return Ok(state);
        }
        thread::sleep(interval.max(Duration::from_millis(10)));
    }
}

/// Foreground uses exactly the durable detached state model with an observer thread.
pub fn foreground(run: &Path) -> Result<RunStatus> {
    let path = run.to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let observer_stop = stop.clone();
    let observer = thread::spawn(move || observe_service(&path, &observer_stop));
    let execution = worker(run);
    // The observer detects terminal/lost worker on its own; no follower authority.
    stop.store(true, Ordering::Release);
    let observed = observer
        .join()
        .map_err(|_| error("observer thread panicked"));
    execution?;
    if let Err(e) = observed.and_then(|result| result) {
        let _ = publish(
            &run.join("observer-error.json"),
            &serde_json::json!({"reason": e.to_string(), "unix_seconds": now()}),
        );
    }
    if let Ok(state) = status(run)
        && state.terminal
    {
        let _ = notify_terminal(run, &state);
    }
    status(run)
}

/// Starts two persistent user services with absolute executable/run paths.
/// CLI must dispatch `worker DIR` / `observe DIR`. Observer restart only attaches.
pub fn start_detached(run: &Path, executable: &Path) -> Result<Vec<String>> {
    load_spec(run)?;
    let service_path = std::env::var("PATH")?;
    evidence::check_path(executable)?;
    if !executable.is_absolute() || !executable.is_file() {
        return Err(error("service executable must be an absolute regular file"));
    }
    let run = run.canonicalize()?;
    let _launch = lease(&run.join("launch.lock"), true)?;
    if run.join("launch-requested.json").exists()
        || run.join("started.json").exists()
        || run.join("launch-failed.json").exists()
    {
        return Err(error(
            "run launch already attempted; automatic rerun is forbidden",
        ));
    }
    let id = run
        .file_name()
        .ok_or_else(|| error("run ID unavailable"))?
        .to_string_lossy();
    if !safe_id(&id) {
        return Err(error("invalid service run ID"));
    }
    // A caller's target/debug binary can be rebuilt while services are alive.
    // Both roles, including observer restarts, execute the same retained bytes.
    let service_executable = run.join("service-executable");
    evidence::atomic_write(&service_executable, &evidence::bounded_read(executable)?)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&service_executable, fs::Permissions::from_mode(0o700))?;
    evidence::open(&service_executable, libc::O_RDONLY, 0)?.sync_all()?;
    let binding = bind_file(&service_executable)?;
    publish(&run.join("service-executable.json"), &binding)?;
    publish(
        &run.join("launch-requested.json"),
        &serde_json::json!({"unix_seconds": now(), "executable": binding, "original_executable": executable, "environment": {"PATH": service_path}}),
    )?;
    let mut units: Vec<String> = Vec::new();
    for role in ["observe", "worker"] {
        let binding = evidence::hash(run.as_os_str().as_encoded_bytes());
        let unit = format!("harbor-db-test-{id}-{}-{role}", &binding[..12]);
        let mut argv: Vec<String> = [
            "systemd-run",
            "--user",
            "--quiet",
            "--collect",
            "--service-type=exec",
            "--unit",
            &unit,
            "--property=UMask=0077",
            "--property=KillMode=control-group",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        // current_exe() retains the unwrapped binary. Both services, including
        // observer restarts, need the wrapper/dev-shell's recorded tool lookup.
        argv.push(format!("--setenv=PATH={service_path}"));
        if role == "observe" {
            argv.push("--property=Restart=on-failure".into());
        }
        argv.extend([
            service_executable
                .to_str()
                .ok_or_else(|| error("executable path is not UTF-8"))?
                .into(),
            role.into(),
            run.to_str()
                .ok_or_else(|| error("run path is not UTF-8"))?
                .into(),
        ]);
        if let Err(e) = evidence::bounded_diagnostic(argv, Duration::from_secs(15)) {
            let reason = format!("starting {role} service failed: {e}");
            publish(
                &run.join("launch-failed.json"),
                &serde_json::json!({"schema": 1, "unix_seconds": now(), "role": role, "reason": reason, "started_units": units}),
            )?;
            for own_unit in &units {
                // Only successfully installed per-run observer services are stopped.
                // No daemon, shared cgroup, or persisted process PID is signalled.
                if let Err(cleanup) = evidence::bounded_diagnostic(
                    vec![
                        "systemctl".into(),
                        "--user".into(),
                        "stop".into(),
                        own_unit.clone(),
                    ],
                    Duration::from_secs(10),
                ) {
                    let _ = publish(
                        &run.join("launch-cleanup-error.json"),
                        &serde_json::json!({"unit": own_unit, "reason": cleanup.to_string()}),
                    );
                }
            }
            return Err(error(reason));
        }
        units.push(unit);
    }
    publish(&run.join("units.json"), &units)?;
    Ok(units)
}
