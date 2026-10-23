//! First-rollout coordinator for a guarded PostgreSQL primary. It only sequences
//! the existing Harbor verbs (startup inhibition, writer fence, preparation,
//! adoption, thaw) behind one resumable journal; every path, unit and executor
//! comes from the immutable rollout manifest.
use super::{Result, codec, durable, invalid, process, writer_fence};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PAUSE_DROP_IN: &str = "zz-harbor-db-rollout-pause.conf";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
/// The journal root is fixed, so every manifest of a primary (including a
/// superseded one) finds the same journal and no manifest can relocate it.
const STATE_ROOT: &str = "/var/lib/harbor-db/postgresql-rollout";
/// The manifest the running generation carries.
const INSTALLED: &str = "/etc/harbor-db/postgresql-rollout.json";
/// One deterministic worker: a retry finds an orphaned preparation by name.
const PREPARE_UNIT: &str = "harbor-db-rollout-prepare.service";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(600);
const HARBOR_TIMEOUT: Duration = Duration::from_secs(1800);
// ponytail: fixed one-day preparation bound; make it a manifest field if a
// cluster ever needs longer.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(86400);
const BUSY: &str = "a preparation worker (harbor-db-rollout-prepare.service) is still running; \
    wait for it (journalctl -u harbor-db-rollout-prepare.service) or stop it, then rerun";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub host: String,
    pub system_identifier: String,
    pub harbor: PathBuf,
    pub systemctl: PathBuf,
    pub systemd_run: PathBuf,
    /// Harbor's fenced maintenance manifest (data_dir, recovery, startup_inhibition).
    pub maintenance: PathBuf,
    /// Harbor's `prepare-recovery --preparation-config` argv file.
    pub preparation: PathBuf,
    /// Host lease every mutating step holds, shared with the consumer's
    /// activations and other database coordinators.
    pub execution_lock: Option<PathBuf>,
    pub socket_dir: PathBuf,
    pub port: u16,
    pub clients: Vec<String>,
    pub retired_clients: Vec<String>,
    pub setup_units: Vec<String>,
    pub receiver_units: Vec<String>,
    pub paused_publishers: Vec<String>,
    pub setup_settle_seconds: u64,
    pub prepare: Prepare,
    pub import_slot: PathBuf,
    pub executor: Executor,
    pub cutover: Option<Cutover>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub export_root: PathBuf,
    pub read_write_paths: Vec<PathBuf>,
    pub required_mounts: Vec<PathBuf>,
    pub supplementary_groups: Vec<String>,
    pub provision_directories: Vec<Provision>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    pub path: PathBuf,
    pub group: String,
    pub mode: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executor {
    pub host: String,
    pub user: String,
    /// Independent restore/certify argv; `--export/--export-sha256/--workspace` are appended.
    pub command: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cutover {
    pub checker: PathBuf,
    pub contract: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Idle,
    Fenced,
    Prepared,
    Imported,
    Adopted,
    Activated,
    Thawed,
    Aborted,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub version: u32,
    pub manifest_sha256: String,
    pub system_identifier: String,
    pub phase: Phase,
    pub clients: Vec<String>,
    pub startup_token: Option<String>,
    pub fence_token: Option<String>,
    pub export: Option<PathBuf>,
    pub export_sha256: Option<String>,
    pub receipt_sha256: Option<String>,
    pub acceptance_sha256: Option<String>,
    /// Receivers quiesced for preparation that still need a restart; retained
    /// so a failed or interrupted cleanup is retried, never forgotten.
    #[serde(default)]
    pub pending_receivers: Vec<String>,
    /// Publishers that were running when paused; restarted after the pause.
    #[serde(default)]
    pub stopped_publishers: Vec<String>,
    /// Recorded once no foreign fence was found and before `fence-open`, so a
    /// fence this rollout opened but never recorded is resumed, not refused.
    #[serde(default)]
    pub fence_opening: bool,
    /// The fence is closed and the normal primary verified; an unwinding retry
    /// only returns clients, receivers and publishers.
    #[serde(default)]
    pub reopened: bool,
}

pub enum Action {
    Status,
    /// Pre-switch guard of a rollout-enabled generation.
    ActivationCheck,
    Next,
    Fence,
    Prepare,
    Certify {
        export: PathBuf,
        export_sha256: String,
        workspace: PathBuf,
    },
    Import {
        receipt: PathBuf,
        sha256: String,
    },
    Adopt,
    Activated,
    Thaw {
        acceptance: PathBuf,
    },
    Abort {
        /// Explicitly adopt Harbor's live fence when the journal is lost.
        adopt_fence_token: Option<String>,
    },
}

pub struct Rollout {
    pub manifest: Manifest,
    pub maintenance: Value,
    pub digest: String,
    config: PathBuf,
    state: PathBuf,
    installed: PathBuf,
    /// Trusted directory the journal ancestry walk starts from (`/` on hosts).
    anchor: PathBuf,
}

fn root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn operate(config: &Path, action: Action) -> Result<Value> {
    let rollout = Rollout::load(&durable::immutable_config_path(config)?)?;
    match action {
        Action::Status => rollout.status(),
        Action::ActivationCheck => rollout.activation_check(),
        Action::Certify {
            export,
            export_sha256,
            workspace,
        } => rollout.certify(&export, &export_sha256, &workspace),
        action => {
            if !root() {
                return Err(invalid("rollout steps require root on the primary host"));
            }
            if fs::read_to_string("/proc/sys/kernel/hostname")?.trim() != rollout.manifest.host {
                return Err(invalid("rollout must run on its declared primary host"));
            }
            rollout.run(action)
        }
    }
}

fn unit_matches(pattern: &str, unit: &str) -> bool {
    match pattern.strip_suffix("@.service") {
        Some(prefix) => unit
            .strip_prefix(&format!("{prefix}@"))
            .is_some_and(|rest| rest != ".service" && rest.ends_with(".service")),
        None => pattern == unit,
    }
}

/// A bounded subprocess in its own process group; failures carry its stderr.
fn spawn_bounded(
    argv: Vec<String>,
    identity: Option<process::Identity>,
    timeout: Duration,
) -> Result<Output> {
    let label = argv.join(" ");
    let mut spec = process::CommandSpec::new(argv);
    spec.identity = identity;
    spec.timeout = timeout;
    spec.capture_stderr = true;
    // A dropped identity may not be able to enter the operator's directory.
    spec.cwd = Some(PathBuf::from("/"));
    process::run(&spec).map_err(|error| invalid(format!("`{label}`: {error}")))
}

fn checked(output: Output) -> Result<Output> {
    if !output.status.success() {
        return Err(invalid(format!(
            "rollout command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

/// A normalized absolute path at least three levels deep (`/var/lib/x`), so a
/// rollout never claims `/`, `/etc` or another shared system directory.
fn dedicated(path: &Path) -> bool {
    use std::path::Component;
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.components().count() >= 4
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// A private file this process owns, in a directory no one else can modify.
fn trusted_file(metadata: &fs::Metadata, path: &Path, owner: u32) -> Result<()> {
    let parent = fs::symlink_metadata(path.parent().ok_or_else(|| invalid("path has no parent"))?)?;
    // Owner-only read (0400, as staging emits) or read-write (0600).
    if !metadata.is_file()
        || metadata.uid() != owner
        || !matches!(metadata.mode() & 0o777, 0o400 | 0o600)
        || !parent.is_dir()
        || parent.uid() != owner
        || parent.mode() & 0o022 != 0
    {
        return Err(invalid(format!(
            "{} must be an owner-only (0400/0600) file of uid {owner} in a directory only that owner can modify",
            path.display()
        )));
    }
    Ok(())
}

fn sha_hex(value: &str) -> Result<String> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("an independently retained SHA-256 is required"));
    }
    Ok(value.to_ascii_lowercase())
}

/// A directory opened without following a final symlink.
fn open_directory(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?)
}

/// The PIDs holding a flock on `path`, from /proc/locks. Btrfs reports a
/// per-subvolume device to stat, so an inode-only match is the fallback.
fn lock_holders(path: &Path) -> String {
    let Ok(metadata) = fs::metadata(path) else {
        return "unknown".into();
    };
    let id = format!(
        "{:02x}:{:02x}:{}",
        libc::major(metadata.dev()),
        libc::minor(metadata.dev()),
        metadata.ino()
    );
    let inode = format!(":{}", metadata.ino());
    let table = fs::read_to_string("/proc/locks").unwrap_or_default();
    let holders = |matches: &dyn Fn(&str) -> bool| {
        table
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .filter(|fields| {
                fields.get(1) == Some(&"FLOCK") && fields.get(5).is_some_and(|f| matches(f))
            })
            .filter_map(|fields| fields.get(4).map(|pid| pid.to_string()))
            .collect::<Vec<_>>()
    };
    let mut found = holders(&|field| field == id);
    if found.is_empty() {
        found = holders(&|field| field.ends_with(&inode));
    }
    found.dedup();
    if found.is_empty() {
        "unknown".into()
    } else {
        found.join(",")
    }
}

const DEFERRED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

fn deferred_set() -> libc::sigset_t {
    // SAFETY: the set is local storage initialised by sigemptyset.
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in DEFERRED {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Termination signals held pending for one step, so an interrupted
/// coordinator stops its own worker instead of orphaning it.
struct Deferred(libc::sigset_t);

impl Deferred {
    fn block() -> Result<Self> {
        let set = deferred_set();
        // SAFETY: both sets are valid; only the calling thread's mask changes.
        let mut previous = unsafe { std::mem::zeroed() };
        if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous) } != 0 {
            return Err(invalid("could not defer termination signals"));
        }
        Ok(Self(previous))
    }

    fn pending(&self) -> bool {
        // SAFETY: set is valid writable storage.
        let mut set = unsafe { std::mem::zeroed() };
        let read = unsafe { libc::sigpending(&mut set) } == 0;
        read && DEFERRED
            .iter()
            .any(|signal| unsafe { libc::sigismember(&set, *signal) } == 1)
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        let set = deferred_set();
        let now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Consume the signals the step already honoured, then restore the mask.
        // SAFETY: sigtimedwait polls with a zero timeout; the saved mask is valid.
        unsafe {
            while libc::sigtimedwait(&set, std::ptr::null_mut(), &now) > 0 {}
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
        }
    }
}

impl Rollout {
    pub fn load(config: &Path) -> Result<Self> {
        let manifest: Manifest = serde_json::from_value(durable::read_config_json(config)?)?;
        let maintenance = durable::read_config_json(&manifest.maintenance)?;
        let valid_unit = |unit: &String| {
            unit.ends_with(".service")
                && unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_@.:-".contains(&b))
        };
        let units = manifest
            .clients
            .iter()
            .chain(&manifest.retired_clients)
            .chain(&manifest.setup_units)
            .chain(&manifest.receiver_units)
            .chain(&manifest.paused_publishers);
        if manifest.version != 2
            || manifest.port == 0
            || manifest.clients.is_empty()
            || !manifest
                .system_identifier
                .parse::<u64>()
                .is_ok_and(|v| v > 0)
            || maintenance["recovery"]["system_identifier"] != manifest.system_identifier.as_str()
            || maintenance["recovery"]["require_writer_fence"] != true
            || !units.clone().all(valid_unit)
            || manifest.executor.command.is_empty()
            || !manifest.execution_lock.as_deref().is_none_or(dedicated)
        {
            return Err(invalid("invalid rollout manifest or primary binding"));
        }
        // systemd splits ReadWritePaths/RequiresMountsFor/SupplementaryGroups
        // on whitespace and LoadCredential on `:`, so one entry must never
        // smuggle in another (or a quote, escape or specifier).
        let plain = |text: Option<&str>, extra: &str| {
            text.is_some_and(|text| {
                !text.is_empty()
                    && !text.chars().any(|c| {
                        c.is_whitespace()
                            || c.is_control()
                            || "\\\"'%".contains(c)
                            || extra.contains(c)
                    })
            })
        };
        let prepare = &manifest.prepare;
        if !(prepare.read_write_paths.iter())
            .chain(&prepare.required_mounts)
            .chain([&prepare.export_root])
            .chain(prepare.provision_directories.iter().map(|d| &d.path))
            .all(|path| plain(path.to_str(), ""))
            || !plain(manifest.import_slot.to_str(), ":")
            || !(prepare.supplementary_groups.iter()).all(|group| plain(Some(group), ""))
        {
            return Err(invalid(
                "rollout systemd property values must not contain whitespace, quotes, `\\`, `%` (or `:` in the import slot)",
            ));
        }
        let state = Path::new(STATE_ROOT);
        let export = manifest.prepare.export_root.as_path();
        if !dedicated(export) || overlaps(state, export) {
            return Err(invalid(
                "rollout export root must be a dedicated directory apart from the journal",
            ));
        }
        for protected in ["data_dir", "state_dir"] {
            let path = Path::new(super::string(&maintenance, protected)?);
            if overlaps(state, path) || overlaps(export, path) {
                return Err(invalid(
                    "rollout journal or export root overlaps primary/authority state",
                ));
            }
        }
        // Thaw runs the checker as root: only immutable store inputs qualify.
        if let Some(cutover) = &manifest.cutover {
            for path in [&cutover.checker, &cutover.contract] {
                if !path.starts_with("/nix/store")
                    || !dedicated(path)
                    || durable::immutable_store_reference(path).is_err()
                {
                    return Err(invalid("cutover checker and contract must be store paths"));
                }
            }
        }
        Ok(Self {
            digest: codec::file_digest(config)?,
            maintenance,
            manifest,
            config: config.to_owned(),
            state: PathBuf::from(STATE_ROOT),
            installed: PathBuf::from(INSTALLED),
            anchor: PathBuf::from("/"),
        })
    }

    fn journal_path(&self) -> PathBuf {
        self.state.join("rollout.json")
    }

    fn journal(&self) -> Result<Option<Journal>> {
        let path = self.journal_path();
        // An unreadable journal is an error, never an idle rollout.
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let journal: Journal = serde_json::from_value(durable::read_json(&path)?)?;
        if journal.version != 1 || journal.system_identifier != self.manifest.system_identifier {
            return Err(invalid(
                "retained rollout journal belongs to another primary",
            ));
        }
        Ok(Some(journal))
    }

    fn fresh_journal(&self) -> Journal {
        Journal {
            version: 1,
            manifest_sha256: self.digest.clone(),
            system_identifier: self.manifest.system_identifier.clone(),
            phase: Phase::Idle,
            clients: Vec::new(),
            startup_token: None,
            fence_token: None,
            export: None,
            export_sha256: None,
            receipt_sha256: None,
            acceptance_sha256: None,
            pending_receivers: Vec::new(),
            stopped_publishers: Vec::new(),
            fence_opening: false,
            reopened: false,
        }
    }

    /// Steps are bound to the exact manifest; only `abort` may unwind a journal
    /// opened by another manifest of the same primary.
    fn bound(&self) -> Result<Journal> {
        let journal = self.journal()?.unwrap_or_else(|| self.fresh_journal());
        if journal.manifest_sha256 != self.digest {
            return Err(invalid(
                "retained rollout journal belongs to another manifest; finish or abort it first",
            ));
        }
        Ok(journal)
    }

    /// Fail unless every declared mount covering `path` is mounted.
    fn require_covering_mounts(&self, path: &Path) -> Result<()> {
        let covering: Vec<_> = (self.manifest.prepare.required_mounts.iter())
            .filter(|mount| path.starts_with(mount))
            .collect();
        if covering.is_empty() {
            return Ok(());
        }
        super::pg_core::require_mounts(&json!({ "required_mounts": covering }))
    }

    /// Walk `path` from the anchor (`/` on hosts) without following symlinks.
    /// Every existing component must be a real directory and every ancestor
    /// one that only root or the coordinator can modify (or one of `holders`);
    /// below a sticky directory a component must be theirs too, since its owner
    /// may rename it. So no other user can swap a component for a symlink
    /// before systemd or a root write resolves the path. With `create`, a
    /// missing component is made by one mkdir (intermediates 0755 so service
    /// users can traverse them, the leaf 0700), adjusted through a no-follow
    /// descriptor and never made below a directory others can write. Returns
    /// the leaf when it exists.
    fn walk(&self, path: &Path, holders: &[&Path], create: bool) -> Result<Option<fs::Metadata>> {
        let untrusted = |at: &Path| {
            invalid(format!(
                "{} has untrusted ancestry at {}",
                path.display(),
                at.display()
            ))
        };
        let below = path
            .strip_prefix(&self.anchor)
            .map_err(|_| untrusted(path))?;
        let mut current = self.anchor.clone();
        let mut parent_shared = false;
        let mut leaf_metadata = None;
        for component in std::iter::once(None).chain(below.components().map(Some)) {
            current.extend(component);
            let leaf = current == *path;
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if parent_shared {
                        return Err(untrusted(&current));
                    }
                    let mode = if leaf { 0o700 } else { 0o755 };
                    fs::DirBuilder::new().mode(mode).create(&current)?;
                    let directory = open_directory(&current)?;
                    directory.set_permissions(fs::Permissions::from_mode(mode))?;
                    directory.metadata()?
                }
                Err(error) => return Err(error.into()),
            };
            let owned = [0, euid()].contains(&metadata.uid());
            let shared = metadata.mode() & 0o022 != 0;
            let sticky = metadata.mode() & 0o1000 != 0;
            let holder = holders.contains(&current.as_path());
            if !metadata.is_dir()
                || (parent_shared && !owned)
                || (!leaf && (!(owned || holder) || (shared && !sticky)))
            {
                return Err(untrusted(&current));
            }
            parent_shared = shared;
            leaf_metadata = Some(metadata);
        }
        Ok(leaf_metadata)
    }

    /// Create the journal root privately, but never re-permission an existing
    /// one: it must already be a private directory this process owns.
    fn state_dir(&self) -> Result<()> {
        // Never create the journal on the root filesystem below an absent mount.
        self.require_covering_mounts(&self.state)?;
        let leaf = self
            .walk(&self.state, &[], true)?
            .ok_or_else(|| invalid("rollout journal root is missing"))?;
        if leaf.uid() != euid() || leaf.mode() & 0o077 != 0 {
            return Err(invalid(
                "rollout journal root must be a private directory owned by the coordinator",
            ));
        }
        Ok(())
    }

    /// systemd resolves symlinks in the worker's sandbox paths, granting the
    /// target write access: every one must lie on a chain no one else can swap.
    fn check_sandbox_paths(&self) -> Result<()> {
        let prepare = &self.manifest.prepare;
        for path in (prepare.read_write_paths.iter())
            .chain([&prepare.export_root])
            .chain(&prepare.required_mounts)
        {
            self.walk(path, &[], false)?;
        }
        Ok(())
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        self.state_dir()?;
        durable::write_json(&self.journal_path(), &serde_json::to_value(journal)?)
    }

    fn systemctl(&self, args: &[&str]) -> Result<Output> {
        let mut argv = vec![self.manifest.systemctl.display().to_string()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        checked(spawn_bounded(argv, None, SYSTEMCTL_TIMEOUT)?)
    }

    /// Harbor verbs, as postgres for cluster-owned state; the identity is only
    /// dropped by a root coordinator.
    fn harbor_output(&self, as_postgres: bool, args: &[&str]) -> Result<Output> {
        let identity = if as_postgres && root() {
            Some(process::account("postgres")?)
        } else {
            None
        };
        let mut argv = vec![
            self.manifest.harbor.display().to_string(),
            "--config".into(),
            self.manifest.maintenance.display().to_string(),
        ];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        spawn_bounded(argv, identity, HARBOR_TIMEOUT)
    }

    fn harbor(&self, as_postgres: bool, args: &[&str]) -> Result<Output> {
        checked(self.harbor_output(as_postgres, args)?)
    }

    fn response(&self, as_postgres: bool, args: &[&str], status: &str) -> Result<Value> {
        let value = codec::decode(&self.harbor(as_postgres, args)?.stdout)?;
        if value["status"] != status {
            return Err(invalid(format!(
                "unexpected Harbor response; expected {status}"
            )));
        }
        Ok(value)
    }

    fn primary_unit(&self) -> &str {
        self.maintenance["startup_inhibition"]["unit"]
            .as_str()
            .unwrap_or("postgresql.service")
    }

    fn socket(&self) -> String {
        self.manifest.socket_dir.display().to_string()
    }

    fn show(&self, unit: &str, property: &str) -> Result<String> {
        let output =
            self.systemctl(&["show", unit, "--value", &format!("--property={property}")])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Whether the deterministic preparation worker is still loaded and running.
    /// A never-loaded or collected (`--collect`) worker reads `not-found`
    /// (systemctl still exits 0) and is idle; any other unknown state is busy.
    fn preparing(&self) -> Result<bool> {
        let output = self.systemctl(&["show", PREPARE_UNIT, "--property=LoadState,ActiveState"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap_or_default()
                .to_owned()
        };
        Ok(value("LoadState=") != "not-found"
            && !matches!(value("ActiveState=").as_str(), "inactive" | "failed"))
    }

    fn active_clients(&self) -> Result<Vec<String>> {
        let output = self.systemctl(&[
            "list-units",
            "--type=service",
            "--state=active,activating,reloading,deactivating",
            "--plain",
            "--no-legend",
            "--no-pager",
        ])?;
        let mut clients: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|unit| {
                self.manifest
                    .clients
                    .iter()
                    .chain(&self.manifest.retired_clients)
                    .any(|pattern| unit_matches(pattern, unit))
            })
            .map(str::to_owned)
            .collect();
        clients.sort();
        clients.dedup();
        Ok(clients)
    }

    /// A deliberate start: clear the start limit earlier rollout cycles may have
    /// spent, or a retried unwind leaves the stopped unit down.
    fn start(&self, unit: &str) -> Result<()> {
        let _ = self.systemctl(&["reset-failed", "--", unit]);
        self.systemctl(&["start", "--", unit]).map(drop)
    }

    fn stop(&self, unit: &str) -> Result<()> {
        self.systemctl(&["stop", "--", unit])?;
        match self.show(unit, "ActiveState")?.as_str() {
            "inactive" | "failed" => Ok(()),
            state => Err(invalid(format!("{unit} did not stop ({state})"))),
        }
    }

    /// Start the primary, then wait for the declared setup units: they connect
    /// as the control role right after startup and must not race inspection.
    fn start_primary(&self) -> Result<()> {
        self.start(self.primary_unit())?;
        let deadline = Instant::now() + Duration::from_secs(self.manifest.setup_settle_seconds);
        loop {
            let mut busy = false;
            for unit in &self.manifest.setup_units {
                let state = self.show(unit, "ActiveState")?;
                busy |= !self.show(unit, "Job")?.is_empty()
                    || matches!(state.as_str(), "activating" | "deactivating" | "reloading");
            }
            if !busy {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(invalid(
                    "setup units did not settle; retain the writer window and retry",
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn inhibit(&self, journal: &mut Journal) -> Result<()> {
        let value = self.response(
            false,
            &[
                "inhibit-startup",
                "--system-identifier",
                &self.manifest.system_identifier,
            ],
            "startup-inhibited",
        )?;
        journal.startup_token = Some(super::string(&value, "token")?.to_owned());
        self.save(journal)
    }

    fn release_startup(&self, journal: &Journal, phase: &str) -> Result<()> {
        self.response(
            false,
            &[
                "release-startup",
                "--token",
                journal.startup_token.as_deref().unwrap_or_default(),
                "--fence-token",
                journal.fence_token.as_deref().unwrap_or_default(),
                "--phase",
                phase,
            ],
            "startup-released",
        )?;
        Ok(())
    }

    fn inspect_fence(&self, journal: &Journal) -> Result<()> {
        let value = self.response(
            true,
            &[
                "inspect-fence",
                "--token",
                journal.fence_token.as_deref().unwrap_or_default(),
                "--socket-dir",
                &self.socket(),
                "--port",
                &self.manifest.port.to_string(),
            ],
            "ready",
        )?;
        if value["system_identifier"] != self.manifest.system_identifier.as_str() {
            return Err(invalid("live fence readback differs"));
        }
        Ok(())
    }

    /// Verify the unfenced primary is this cluster again.
    fn inspect_live(&self) -> Result<()> {
        let live = codec::decode(
            &self
                .harbor(
                    true,
                    &[
                        "inspect-live",
                        "--system-identifier",
                        &self.manifest.system_identifier,
                        "--socket-dir",
                        &self.socket(),
                        "--port",
                        &self.manifest.port.to_string(),
                    ],
                )?
                .stdout,
        )?;
        if live["system_identifier"] != self.manifest.system_identifier.as_str() {
            return Err(invalid("normal primary readback differs"));
        }
        Ok(())
    }

    /// The token of the (shared) startup inhibition Harbor holds, if any.
    fn held_inhibition(&self) -> Result<Option<String>> {
        let state = super::string(&self.maintenance["startup_inhibition"], "state_dir")?;
        let path = Path::new(state).join("inhibited.json");
        if !writer_fence::exists(&path) {
            return Ok(None);
        }
        Ok(Some(
            super::string(&durable::read_json(&path)?, "token")?.to_owned(),
        ))
    }

    /// The token of any writer fence Harbor holds for this primary, whoever
    /// opened it: the active record, or an interrupted acquisition's selector.
    fn live_fence(&self) -> Result<Option<String>> {
        let marker = writer_fence::marker(&self.maintenance)?;
        if writer_fence::exists(&marker) {
            let record = durable::read_json(&marker)?;
            return Ok(Some(super::string(&record, "token")?.to_owned()));
        }
        let auto =
            Path::new(super::string(&self.maintenance, "data_dir")?).join("postgresql.auto.conf");
        let bytes = match fs::read(&auto) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(bytes
            .split(|b| *b == b'\n')
            .find_map(|line| line.strip_prefix(b"# harbor-db-writer-fence="))
            .map(|token| String::from_utf8_lossy(token).into_owned()))
    }

    /// Refuse a live fence this rollout did not open (another coordinator's),
    /// unless it recorded opening one whose token it has yet to learn.
    fn refuse_foreign_fence(&self, journal: &Journal) -> Result<()> {
        if !journal.fence_opening
            && let Some(token) = self.live_fence()?
            && journal.fence_token.as_deref() != Some(token.as_str())
        {
            return Err(invalid(format!(
                "Harbor holds writer fence {token}, which this rollout did not open; \
                 finish it with the coordinator that opened it first"
            )));
        }
        Ok(())
    }

    /// Open (or resume) the writer fence. Harbor resumes any prepared fence, so
    /// first refuse one this rollout did not open (another coordinator's).
    fn open_fence(&self, journal: &mut Journal) -> Result<()> {
        if !journal.fence_opening {
            self.refuse_foreign_fence(journal)?;
            journal.fence_opening = true;
            self.save(journal)?;
        }
        let value = self.response(
            true,
            &[
                "fence-open",
                "--system-identifier",
                &self.manifest.system_identifier,
            ],
            "prepared-offline",
        )?;
        journal.fence_token = Some(super::string(&value, "token")?.to_owned());
        self.save(journal)
    }

    fn pause_marker(&self) -> PathBuf {
        self.state.join("paused")
    }

    fn drop_in_root(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(super::string(
            &self.maintenance["startup_inhibition"],
            "drop_in_root",
        )?))
    }

    /// Publishers are skipped while the marker exists; the drop-in survives
    /// daemon-reload and generation switches, unlike a stopped timer.
    fn pause_publishers(&self, journal: &mut Journal) -> Result<()> {
        if self.manifest.paused_publishers.is_empty() {
            return Ok(());
        }
        let root = self.drop_in_root()?;
        let condition = format!(
            "# Harbor DB rollout publisher pause.\n[Unit]\nConditionPathExists=!{}\n",
            self.pause_marker().display()
        );
        durable::atomic_write_mode(&self.pause_marker(), b"", 0o600)?;
        for unit in &self.manifest.paused_publishers {
            let directory = root.join(format!("{unit}.d"));
            fs::create_dir_all(&directory)?;
            durable::atomic_write_mode(
                &directory.join(PAUSE_DROP_IN),
                condition.as_bytes(),
                0o644,
            )?;
        }
        self.systemctl(&["daemon-reload"])?;
        for unit in &self.manifest.paused_publishers {
            // Recorded before stopping, so abort or thaw restarts it.
            let state = self.show(unit, "ActiveState")?;
            if !matches!(state.as_str(), "inactive" | "failed")
                && !journal.stopped_publishers.contains(unit)
            {
                journal.stopped_publishers.push(unit.clone());
                self.save(journal)?;
            }
            self.stop(unit)?;
        }
        Ok(())
    }

    fn lift_pause(&self, journal: &mut Journal) -> Result<()> {
        // Publishers must not snapshot a cluster a preparation is still copying.
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        let mut lifted = false;
        if !self.manifest.paused_publishers.is_empty() {
            let root = self.drop_in_root()?;
            for unit in &self.manifest.paused_publishers {
                let file = root.join(format!("{unit}.d")).join(PAUSE_DROP_IN);
                if file.exists() {
                    fs::remove_file(file)?;
                    lifted = true;
                }
            }
        }
        // The marker gates every manifest's drop-ins (a cross-manifest abort may
        // declare other publishers), so it always goes.
        if self.pause_marker().exists() {
            fs::remove_file(self.pause_marker())?;
            lifted = true;
        }
        if lifted {
            self.systemctl(&["daemon-reload"])?;
        }
        let restarted = self.restart(&mut journal.stopped_publishers);
        self.save(journal)?;
        restarted
    }

    fn stop_clients(&self, journal: &mut Journal) -> Result<()> {
        journal.clients.extend(self.active_clients()?);
        journal.clients.sort();
        journal.clients.dedup();
        self.save(journal)?;
        for unit in &journal.clients {
            self.stop(unit)?;
        }
        Ok(())
    }

    /// Create a service directory as root without ever following a symlink.
    fn provision(&self, path: &Path, group: &str, mode: u32) -> Result<()> {
        // Never provision on the root filesystem below an absent mount.
        self.require_covering_mounts(path)?;
        // A declared directory may hold further ones (an evidence directory in
        // its backup root): those are made by mkdir inside it and adjusted
        // through a no-follow descriptor, so its owner cannot redirect them.
        let holders: Vec<&Path> = (self.manifest.prepare.provision_directories.iter())
            .map(|directory| directory.path.as_path())
            .collect();
        if self.walk(path, &holders, false)?.is_some() {
            return Ok(());
        }
        let (uid, _) = account("postgres")?;
        let gid = group_id(group)?;
        self.walk(path, &holders, true)?;
        let directory = open_directory(path)?;
        directory.set_permissions(fs::Permissions::from_mode(mode))?;
        std::os::unix::fs::fchown(&directory, Some(uid), Some(gid))?;
        Ok(())
    }

    fn fence(&self, journal: &mut Journal) -> Result<()> {
        if journal.phase != Phase::Idle {
            // Resumed: re-stop any newly started declared client and re-verify.
            self.stop_clients(journal)?;
            return self.inspect_fence(journal);
        }
        // Before any side effect: a refusal must leave production untouched.
        self.refuse_foreign_fence(journal)?;
        let authority = Path::new(super::string(&self.maintenance, "state_dir")?);
        self.provision(authority, "postgres", 0o700)?;
        self.inhibit(journal)?;
        self.pause_publishers(journal)?;
        self.stop_clients(journal)?;
        self.stop(self.primary_unit())?;
        self.open_fence(journal)?;
        self.release_startup(journal, "prepared")?;
        self.start_primary()?;
        self.inspect_fence(journal)?;
        journal.phase = Phase::Fenced;
        self.save(journal)
    }

    fn evidence_dir(&self) -> Result<PathBuf> {
        Path::new(super::string(
            &self.maintenance["recovery"],
            "snapshot_file",
        )?)
        .parent()
        .map(Path::to_owned)
        .ok_or_else(|| invalid("snapshot file has no parent"))
    }

    fn preparation_command(&self, import: bool) -> Command {
        let prepare = &self.manifest.prepare;
        let mut command = Command::new(&self.manifest.systemd_run);
        command
            .args(["--quiet", "--wait", "--pipe", "--collect"])
            .arg(format!("--unit={PREPARE_UNIT}"))
            .args([
                "--property=Type=oneshot",
                "--property=User=postgres",
                "--property=Group=postgres",
                "--property=ProtectSystem=strict",
                "--property=PrivateTmp=true",
                "--property=NoNewPrivileges=true",
                "--property=UMask=0077",
                "--property=KillMode=control-group",
                "--property=CPUWeight=10",
            ])
            .arg(format!(
                "--property=TimeoutStartSec={}",
                PREPARE_TIMEOUT.as_secs()
            ));
        let join = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        if !prepare.supplementary_groups.is_empty() {
            command.arg(format!(
                "--property=SupplementaryGroups={}",
                prepare.supplementary_groups.join(" ")
            ));
        }
        // The worker publishes the hand-off export under the export root.
        let mut writable = prepare.read_write_paths.clone();
        if !writable.contains(&prepare.export_root) {
            writable.push(prepare.export_root.clone());
        }
        command.arg(format!("--property=ReadWritePaths={}", join(&writable)));
        if !prepare.required_mounts.is_empty() {
            command.arg(format!(
                "--property=RequiresMountsFor={}",
                join(&prepare.required_mounts)
            ));
        }
        if import {
            command.arg(format!(
                "--property=LoadCredential=recovery-off-host:{}",
                self.manifest.import_slot.display()
            ));
        }
        command
            .arg("--")
            .arg(&self.manifest.harbor)
            .arg("--config")
            .arg(&self.manifest.maintenance)
            .arg("prepare-recovery")
            .arg("--preparation-config")
            .arg(&self.manifest.preparation)
            .arg("--socket-dir")
            .arg(&self.manifest.socket_dir)
            .args(["--port", &self.manifest.port.to_string()]);
        // Only the coordinator sees a terminal interrupt; the worker's own JSON
        // goes to stderr, so a step prints exactly one status document.
        command
            .stdin(Stdio::null())
            .stdout(std::io::stderr())
            .process_group(0);
        command
    }

    /// Restart the retained receivers; those that fail stay pending for retry.
    /// Nothing restarts while a preparation worker still runs.
    fn restart_receivers(&self, journal: &mut Journal) -> Result<()> {
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        let restarted = self.restart(&mut journal.pending_receivers);
        self.save(journal)?;
        restarted
    }

    /// Start every unit; those that fail stay listed for the next retry.
    fn restart(&self, units: &mut Vec<String>) -> Result<()> {
        let mut first = None;
        units.retain(|unit| match self.start(unit) {
            Ok(()) => false,
            Err(error) => {
                first.get_or_insert(error);
                true
            }
        });
        first.map_or(Ok(()), Err)
    }

    /// Wait for the worker, stopping its unit on an interrupt or the deadline.
    fn await_worker(&self, import: bool, signals: &Deferred) -> Result<bool> {
        let mut command = self.preparation_command(import);
        let original = signals.0;
        // SAFETY: sigprocmask is async-signal-safe and the saved mask is plain
        // data; the client must not inherit the coordinator's deferral.
        unsafe {
            command.pre_exec(move || {
                libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = process::spawn(&mut command)?;
        let deadline = Instant::now() + PREPARE_TIMEOUT + Duration::from_secs(60);
        loop {
            let exited = child.try_wait()?;
            let interrupted = signals.pending();
            if interrupted || (exited.is_none() && Instant::now() >= deadline) {
                // The unit may only now be starting: stop it until its client
                // returns. A unit that outlives this stays `BUSY` for receivers.
                for _ in 0..60 {
                    let _ = self.systemctl(&["stop", "--", PREPARE_UNIT]);
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(invalid(if interrupted {
                    "preparation interrupted; its worker was stopped"
                } else {
                    "preparation exceeded its execution limit; its worker was stopped"
                }));
            }
            if let Some(status) = exited {
                return Ok(status.success());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Run Harbor's preparation with the installed receivers stopped. Every
    /// receiver quiesced here is recorded before it is stopped and restarted
    /// whatever the outcome; a failed restart stays recorded for the next run.
    /// Returns whether preparation reported ready.
    fn run_preparation(&self, journal: &mut Journal, import: bool) -> Result<bool> {
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        for directory in &self.manifest.prepare.provision_directories {
            self.provision(&directory.path, &directory.group, directory.mode)?;
        }
        self.check_sandbox_paths()?;
        let signals = Deferred::block()?;
        let quiesced = (|| {
            for unit in &self.manifest.receiver_units {
                // A transitional receiver can still be writing WAL; quiesce every
                // non-terminal state, and resume those that were (going) up.
                let state = self.show(unit, "ActiveState")?;
                if !matches!(state.as_str(), "inactive" | "failed") {
                    if state != "deactivating" && !journal.pending_receivers.contains(unit) {
                        journal.pending_receivers.push(unit.clone());
                        self.save(journal)?;
                    }
                    self.stop(unit)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = quiesced {
            let _ = self.restart_receivers(journal);
            return Err(error);
        }
        let ready = self.await_worker(import, &signals);
        self.restart_receivers(journal)?;
        ready
    }

    fn prepare(&self, journal: &mut Journal) -> Result<()> {
        self.inspect_fence(journal)?;
        let import = self.import_slot(journal)?;
        if self.run_preparation(journal, import)? {
            journal.phase = Phase::Imported;
            return self.save(journal);
        }
        // Harbor stops before admission until the independent receipt exists;
        // the retained export is then the hand-off to the executor.
        let preparation = durable::read_json(&self.evidence_dir()?.join("preparation.json"))?;
        let export = self
            .manifest
            .prepare
            .export_root
            .join(super::string(&preparation, "backup_id")?);
        let plan = export.join("recovery-export.json");
        if import || !plan.is_file() {
            return Err(invalid(
                "recovery preparation failed; retain the writer window and inspect its unit",
            ));
        }
        journal.export_sha256 = Some(codec::file_digest(&plan)?);
        journal.export = Some(export);
        journal.phase = Phase::Prepared;
        self.save(journal)
    }

    /// Whether a receipt that `rollout import` staged and recorded is waiting.
    /// An unrecorded or untrusted slot is never handed to the root-launched worker.
    fn import_slot(&self, journal: &Journal) -> Result<bool> {
        let slot = &self.manifest.import_slot;
        let metadata = match fs::symlink_metadata(slot) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        trusted_file(&metadata, slot, euid())?;
        if journal.receipt_sha256.as_deref() != Some(codec::file_digest(slot)?.as_str()) {
            return Err(invalid(
                "import slot holds an unrecorded receipt; stage it with `rollout import`",
            ));
        }
        Ok(true)
    }

    fn import(&self, journal: &mut Journal, receipt: &Path, expected: &str) -> Result<()> {
        let expected = sha_hex(expected)?;
        let metadata = fs::symlink_metadata(receipt)?;
        if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
            return Err(invalid(
                "transported receipt must be a private mode-0600 regular file",
            ));
        }
        let mut bytes = Vec::new();
        fs::File::open(receipt)?
            .take(MAX_RECEIPT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES
            || codec::digest(&bytes) != expected
            || !codec::decode(&bytes)?.is_object()
        {
            return Err(invalid(
                "transported receipt differs from the independently retained SHA-256",
            ));
        }
        // Stage the verified bytes, never a pathname the transport can still swap.
        // An interrupted import may already have staged these exact bytes;
        // reuse them, but never replace a different staged receipt.
        let slot = &self.manifest.import_slot;
        if let Ok(metadata) = fs::symlink_metadata(slot) {
            trusted_file(&metadata, slot, euid())?;
            if codec::file_digest(slot)? != expected {
                return Err(invalid(
                    "import slot holds a different receipt; retain the writer window",
                ));
            }
        } else {
            let verified = self.state.join("receipt.verified.json");
            durable::atomic_write_mode(&verified, &bytes, 0o600)?;
            super::recovery::stage_off_host_import(&verified, slot)?;
        }
        journal.receipt_sha256 = Some(expected);
        self.save(journal)?;
        self.inspect_fence(journal)?;
        if !self.run_preparation(journal, true)? {
            return Err(invalid(
                "independent receipt was not admitted; retain the writer window",
            ));
        }
        journal.phase = Phase::Imported;
        self.save(journal)
    }

    fn adopt(&self, journal: &mut Journal) -> Result<()> {
        let socket = self.socket();
        let port = self.manifest.port.to_string();
        // Live records must still equal the accepted snapshot before authority.
        self.response(
            true,
            &["inspect-recovery", "--socket-dir", &socket, "--port", &port],
            "ready",
        )?;
        self.harbor(
            true,
            &[
                "adopt-live",
                "--system-identifier",
                &self.manifest.system_identifier,
                "--socket-dir",
                &socket,
                "--port",
                &port,
            ],
        )?;
        self.harbor(true, &["check"])?;
        journal.phase = Phase::Adopted;
        self.save(journal)
    }

    /// Unwind to the normal primary: close (or confirm closed) the fence under
    /// startup exclusion, restart, verify, then return recorded clients.
    fn reopen(&self, journal: &mut Journal) -> Result<()> {
        if !journal.reopened {
            if let (None, Some(startup)) = (&journal.fence_token, journal.startup_token.clone()) {
                // Interrupted between inhibition and fence-open. Harbor binds the
                // startup release to a fence boundary, so complete the offline
                // acquisition first and then unwind it through the normal close.
                let foreign = self.live_fence()?.filter(|_| !journal.fence_opening);
                if self.held_inhibition()?.as_deref() != Some(startup.as_str()) {
                    // The shared inhibition was already released at another
                    // coordinator's fence boundary: only restart the primary
                    // this rollout stopped (a foreign fence is that one's to verify).
                    self.start_primary()?;
                    if foreign.is_none() {
                        self.inspect_live()?;
                    }
                } else if let Some(foreign) = foreign {
                    return Err(invalid(format!(
                        "this rollout holds startup inhibition {startup} but Harbor holds writer fence \
                         {foreign}, which it did not open; Harbor releases startup only at a fence \
                         boundary, so finish fence {foreign} with the coordinator that opened it (that \
                         releases the shared inhibition), then rerun `rollout abort`"
                    )));
                } else {
                    self.stop(self.primary_unit())?;
                    self.open_fence(journal)?;
                    self.release_startup(journal, "prepared")?;
                }
            }
            if let Some(token) = journal.fence_token.clone() {
                self.inhibit(journal)?;
                self.stop(self.primary_unit())?;
                let closed = self.harbor_output(
                    true,
                    &[
                        "inspect-offline-fence",
                        "--token",
                        &token,
                        "--phase",
                        "closed",
                    ],
                )?;
                if !closed.status.success() {
                    self.response(true, &["fence-close", "--token", &token], "closed-offline")?;
                }
                self.release_startup(journal, "closed")?;
                self.start_primary()?;
                self.inspect_live()?;
            }
            journal.reopened = true;
            self.save(journal)?;
        }
        for unit in &journal.clients {
            if self
                .manifest
                .retired_clients
                .iter()
                .any(|pattern| unit_matches(pattern, unit))
            {
                continue;
            }
            self.start(unit)?;
        }
        self.restart_receivers(journal)?;
        self.lift_pause(journal)
    }

    /// `activated` and `thaw` act on the deployed generation, so it must carry
    /// exactly this manifest.
    fn require_installed(&self) -> Result<()> {
        if fs::canonicalize(&self.installed).ok() != Some(fs::canonicalize(&self.config)?) {
            return Err(invalid(format!(
                "the running generation's {} is not this manifest; activate the candidate first",
                self.installed.display()
            )));
        }
        Ok(())
    }

    fn thaw(&self, journal: &mut Journal, acceptance: &Path) -> Result<()> {
        // Validate, digest and parse one O_NOFOLLOW descriptor, never a
        // pathname that could be swapped between the checks and the reads.
        use std::os::unix::fs::OpenOptionsExt;
        let not_root_owned = || invalid("acceptance must be a root-owned mode-0600 regular file");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(acceptance)
            .map_err(|_| not_root_owned())?;
        let metadata = file.metadata()?;
        if metadata.uid() != 0 {
            return Err(not_root_owned());
        }
        trusted_file(&metadata, acceptance, 0).map_err(|_| not_root_owned())?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_RECEIPT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(not_root_owned());
        }
        let digest = codec::digest(&bytes);
        let accepted = codec::decode(&bytes)?;
        let snapshot = Path::new(super::string(
            &self.maintenance["recovery"],
            "snapshot_file",
        )?);
        if accepted["version"] != 1
            || accepted["status"] != "accepted"
            || accepted["host"] != self.manifest.host.as_str()
            || accepted["system_identifier"] != self.manifest.system_identifier.as_str()
            || accepted["manifest_sha256"] != self.digest.as_str()
            || accepted["token"].as_str() != journal.fence_token.as_deref()
            || accepted["snapshot_sha256"] != codec::file_digest(snapshot)?.as_str()
        {
            return Err(invalid(
                "acceptance differs from host/primary/manifest/fence/snapshot",
            ));
        }
        match &journal.acceptance_sha256 {
            Some(retained) if *retained != digest => {
                return Err(invalid("interrupted thaw requires the original acceptance"));
            }
            Some(_) => {}
            None => {
                self.inspect_fence(journal)?;
                self.response(true, &["inspect-recovery"], "ready")?;
                if let Some(cutover) = &self.manifest.cutover {
                    checked(spawn_bounded(
                        vec![
                            cutover.checker.display().to_string(),
                            "check".into(),
                            "--contract".into(),
                            cutover.contract.display().to_string(),
                            "--host".into(),
                            self.manifest.host.clone(),
                            "--phase".into(),
                            "certify".into(),
                        ],
                        None,
                        HARBOR_TIMEOUT,
                    )?)?;
                }
                journal.acceptance_sha256 = Some(digest);
                self.save(journal)?;
            }
        }
        self.reopen(journal)?;
        journal.phase = Phase::Thawed;
        self.save(journal)
    }

    fn certify(&self, export: &Path, expected: &str, workspace: &Path) -> Result<Value> {
        let host = fs::read_to_string("/proc/sys/kernel/hostname")?;
        let user = super::accounts::name(
            unsafe { libc::getuid() },
            4096,
            "executor user is absent",
            "executor user is malformed",
        )?;
        if host.trim() != self.manifest.executor.host
            || user != self.manifest.executor.user
            || root()
        {
            return Err(invalid(
                "certify must run as the declared unprivileged executor",
            ));
        }
        let (program, args) = self
            .manifest
            .executor
            .command
            .split_first()
            .ok_or_else(|| invalid("empty executor command"))?;
        let status = Command::new(program)
            .args(args)
            .arg("--export")
            .arg(export)
            .arg("--export-sha256")
            .arg(sha_hex(expected)?)
            .arg("--workspace")
            .arg(workspace)
            // Restore chatter (pg_ctl, its receipt) must not join the step's JSON.
            .stdout(std::io::stderr())
            .status()?;
        if !status.success() {
            return Err(invalid("independent certification failed; import nothing"));
        }
        Ok(
            json!({"status":"certified","receipt":workspace.join("off-host-recovery.json"),
            "next":"transport the receipt and its SHA-256 to the primary, then run `rollout import`"}),
        )
    }

    fn next_hint(&self, journal: &Journal) -> Value {
        let config = self.config.display();
        let executor = &self.manifest.executor;
        match journal.phase {
            // The executor needs this manifest's store closure (Harbor and the
            // certify argv it names) as well as the export itself.
            Phase::Prepared => json!(format!(
                "copy the manifest closure to {host}: `nix copy --to ssh-ng://{host} {config}`; \
                 transfer {export} to {host} as a directory only {user} can read; then on {host} as {user}: \
                 {harbor} --config {config} rollout certify --export <copied export> --export-sha256 {sha} \
                 --workspace <fresh dir>; then here: `rollout import --receipt <receipt> --sha256 <sha>`",
                host = executor.host,
                user = executor.user,
                harbor = self.manifest.harbor.display(),
                export = journal
                    .export
                    .as_deref()
                    .map(Path::display)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
                sha = journal.export_sha256.as_deref().unwrap_or_default(),
            )),
            Phase::Adopted => {
                json!("activate the candidate generation, then run `rollout activated`")
            }
            Phase::Activated => {
                json!("verify the deployment, then run `rollout thaw --acceptance <receipt>`")
            }
            Phase::Thawed => json!(
                "thawed; `rollout fence` (or `next`) under any manifest archives this journal and starts a fresh rollout"
            ),
            Phase::Aborted => json!(
                "aborted; `rollout fence` (or `next`) archives this journal and starts a fresh rollout"
            ),
            _ if journal.reopened => json!("rerun `rollout abort` or `rollout thaw`"),
            _ => json!("run `rollout next`"),
        }
    }

    /// Read-only: takes no lease, but needs read access to the root-owned journal.
    fn status(&self) -> Result<Value> {
        let mut value = match self.journal()? {
            None => {
                json!({"phase":"idle","manifest_sha256":self.digest,"next":"run `rollout next`"})
            }
            Some(journal) => {
                let next = self.next_hint(&journal);
                let mut value = serde_json::to_value(&journal)?;
                value["next"] = next;
                value["bound"] = json!(journal.manifest_sha256 == self.digest);
                value
            }
        };
        if self.preparing()? {
            value["preparation_worker"] = json!(PREPARE_UNIT);
            value["next"] = json!(BUSY);
        }
        let archived = self.archives()?;
        if !archived.is_empty() {
            value["archived"] = json!(archived);
        }
        Ok(value)
    }

    /// Replaces the legacy pre-switch preparation for a rollout-enabled
    /// generation: the coordinator prepares and adopts under the fence, so
    /// activation is refused only while a rollout sits between fence and
    /// adoption, or was adopted under another manifest. Recovery admission
    /// (`inspect-recovery`) still gates the switch separately.
    fn activation_check(&self) -> Result<Value> {
        if let Some(journal) = self.journal()? {
            let started = journal.startup_token.is_some() || journal.fence_token.is_some();
            match journal.phase {
                Phase::Fenced | Phase::Prepared | Phase::Imported => {
                    return Err(invalid(
                        "the rollout is between fence and adoption; finish it or `rollout abort` before activating",
                    ));
                }
                Phase::Idle if started => {
                    return Err(invalid(
                        "an interrupted rollout fence is pending; rerun `rollout fence` or `rollout abort` before activating",
                    ));
                }
                Phase::Adopted if journal.manifest_sha256 != self.digest => {
                    return Err(invalid(
                        "the rollout was adopted under another manifest; activate that candidate",
                    ));
                }
                _ => {}
            }
        }
        self.status()
    }

    /// Journals of finished rollouts, archived beside the live journal.
    fn archives(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.state) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.starts_with("rollout.")
                    && (name.ends_with(".aborted.json") || name.ends_with(".thawed.json"))
            })
            .collect();
        names.sort();
        Ok(names)
    }

    /// A finished rollout (thawed, or aborted and fully unwound) is archived so
    /// any candidate, under any manifest, can start afresh; an unwind that is
    /// unrecorded or under a held fence never is.
    fn archive_finished(&self) -> Result<()> {
        let Some(journal) = self.journal()? else {
            return Ok(());
        };
        let kind = match journal.phase {
            Phase::Thawed => "thawed",
            Phase::Aborted => {
                if let Some(token) = self.live_fence()? {
                    return Err(invalid(format!(
                        "the aborted rollout is not fully unwound: Harbor holds writer fence {token}"
                    )));
                }
                if !journal.reopened {
                    return Err(invalid(
                        "the aborted rollout is not fully unwound: its reopen is unrecorded",
                    ));
                }
                "aborted"
            }
            _ => return Ok(()),
        };
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid("clock precedes the epoch"))?
            .as_secs();
        // A hard link never replaces an earlier archive.
        fs::hard_link(
            self.journal_path(),
            self.state.join(format!("rollout.{seconds}.{kind}.json")),
        )?;
        fs::remove_file(self.journal_path())?;
        durable::sync_directory(&self.state)
    }

    fn execution_lease(&self) -> Result<Option<durable::Lease>> {
        let Some(path) = &self.manifest.execution_lock else {
            return Ok(None);
        };
        durable::lock(path, false, false)
            .map(Some)
            .map_err(|error| {
                invalid(format!(
                    "execution lock {} is unavailable (held by pid {}): {error}",
                    path.display(),
                    lock_holders(path)
                ))
            })
    }

    fn abort(&self, adopt_fence_token: Option<String>) -> Result<Value> {
        let adopted = adopt_fence_token.is_some();
        let mut journal = match (self.journal()?, adopt_fence_token) {
            (Some(journal), None) => journal,
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "a rollout journal exists; abort it without --adopt-fence-token",
                ));
            }
            (None, None) => {
                return Err(invalid(match self.live_fence()? {
                    Some(token) => format!(
                        "no rollout journal, but Harbor holds writer fence {token} ({}); verify it with \
                         `inspect-fence --token {token}` and, if no other coordinator owns it, rerun \
                         `rollout abort --adopt-fence-token {token}`",
                        writer_fence::marker(&self.maintenance)?.display()
                    ),
                    None => "no rollout to abort".into(),
                }));
            }
            (None, Some(token)) => {
                if self.live_fence()?.as_deref() != Some(token.as_str()) {
                    return Err(invalid(
                        "--adopt-fence-token differs from Harbor's live writer fence",
                    ));
                }
                // The lost journal's stopped clients are unknown; every declared
                // receiver resumes. Reopening re-reads the startup token.
                let mut journal = self.fresh_journal();
                journal.phase = Phase::Fenced;
                journal.fence_token = Some(token);
                journal.fence_opening = true;
                journal.pending_receivers = self.manifest.receiver_units.clone();
                // Which publishers the pause stopped is unknown too: resume them
                // all (an idle timer-driven one just runs early).
                journal.stopped_publishers = self.manifest.paused_publishers.clone();
                self.save(&journal)?;
                journal
            }
        };
        if matches!(journal.phase, Phase::Activated | Phase::Thawed) {
            return Err(invalid(
                "the candidate is activated; thaw with an acceptance instead of aborting",
            ));
        }
        if journal.phase != Phase::Aborted {
            self.reopen(&mut journal)?;
            journal.phase = Phase::Aborted;
            self.save(&journal)?;
        }
        let mut status = self.status()?;
        if adopted {
            status["next"] = json!(
                "restart the writer clients the lost journal had stopped (re-activate the current generation)"
            );
        }
        Ok(status)
    }

    pub fn run(&self, action: Action) -> Result<Value> {
        self.state_dir()?;
        let _lease = durable::lock(&self.state.join("rollout.lock"), false, true)?;
        let _execution = self.execution_lease()?;
        if let Action::Abort { adopt_fence_token } = action {
            return self.abort(adopt_fence_token);
        }
        if matches!(action, Action::Fence | Action::Next) {
            self.archive_finished()?;
        }
        let mut journal = self.bound()?;
        let unwinding =
            journal.reopened && !matches!(journal.phase, Phase::Aborted | Phase::Thawed);
        if unwinding && !matches!(action, Action::Thaw { .. }) {
            return Err(invalid(
                "the rollout is unwinding; rerun `rollout abort` or `rollout thaw`",
            ));
        }
        let phase = journal.phase;
        let require = |allowed: &[Phase]| {
            if allowed.contains(&phase) {
                Ok(())
            } else {
                Err(invalid(format!("rollout is {phase:?}; step not allowed")))
            }
        };
        match action {
            Action::Fence => {
                require(&[Phase::Idle, Phase::Fenced])?;
                self.fence(&mut journal)?;
            }
            Action::Prepare => {
                require(&[Phase::Fenced, Phase::Prepared])?;
                self.prepare(&mut journal)?;
            }
            Action::Import { receipt, sha256 } => {
                require(&[Phase::Prepared])?;
                self.import(&mut journal, &receipt, &sha256)?;
            }
            Action::Adopt => {
                require(&[Phase::Imported])?;
                self.adopt(&mut journal)?;
            }
            Action::Activated => {
                require(&[Phase::Adopted])?;
                self.require_installed()?;
                self.inspect_fence(&journal)?;
                journal.phase = Phase::Activated;
                self.save(&journal)?;
            }
            Action::Thaw { acceptance } => {
                require(&[Phase::Activated, Phase::Thawed])?;
                if phase == Phase::Activated {
                    self.require_installed()?;
                    self.thaw(&mut journal, &acceptance)?;
                }
            }
            Action::Next => match phase {
                Phase::Idle => self.fence(&mut journal)?,
                Phase::Fenced => self.prepare(&mut journal)?,
                // External hand-off: certification runs on the executor; `next`
                // prints the exact commands.
                Phase::Prepared => {}
                Phase::Imported => self.adopt(&mut journal)?,
                _ => {}
            },
            Action::Status
            | Action::ActivationCheck
            | Action::Certify { .. }
            | Action::Abort { .. } => unreachable!(),
        }
        self.status()
    }
}

fn account(user: &str) -> Result<(u32, u32)> {
    let name = std::ffi::CString::new(user).map_err(|_| invalid("invalid user"))?;
    // SAFETY: getpwnam returns null or a pointer valid until the next call;
    // both fields are copied immediately.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return Err(invalid(format!("user {user} is absent")));
    }
    Ok(unsafe { ((*entry).pw_uid, (*entry).pw_gid) })
}

fn group_id(group: &str) -> Result<u32> {
    let name = std::ffi::CString::new(group).map_err(|_| invalid("invalid group"))?;
    // SAFETY: as above for getgrnam.
    let entry = unsafe { libc::getgrnam(name.as_ptr()) };
    if entry.is_null() {
        return Err(invalid(format!("group {group} is absent")));
    }
    Ok(unsafe { (*entry).gr_gid })
}
#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn executable(path: &Path, script: &str) {
        fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        /// Fake systemd keeps one state file per unit; `setup.service` stays
        /// busy for `polls` show queries after every primary start.
        fn new(polls: u32) -> Self {
            let root = tempfile::tempdir().unwrap();
            let p = |name: &str| root.path().join(name);
            for dir in [
                "units",
                "authority",
                "data",
                "evidence",
                "control",
                "state",
                "startup",
            ] {
                fs::create_dir(p(dir)).unwrap();
            }
            fs::set_permissions(p("state"), fs::Permissions::from_mode(0o700)).unwrap();
            for unit in [
                "a.service",
                "b.service",
                "postgresql.service",
                "pub.service",
            ] {
                fs::write(p("units").join(unit), "active").unwrap();
            }
            fs::write(p("units/setup.service"), "inactive").unwrap();
            fs::write(p("evidence/records.json"), "{}").unwrap();
            let log = p("calls.log");
            executable(
                &p("systemctl"),
                &format!(
                    r#"u={units}; log={log}
echo "systemctl $*" >> "$log"
case "$1" in
  list-units) for f in "$u"/*; do [ "$(cat "$f")" = active ] && echo "$(basename "$f") loaded active running x"; done ;;
  stop) echo inactive > "$u/$3" ;;
  reset-failed) rm -f "$u/.starts-$3" ;;
  start) n=$(($(cat "$u/.starts-$3" 2>/dev/null || echo 0) + 1)); echo $n > "$u/.starts-$3"
    [ $n -gt 5 ] && {{ echo "start of the service was attempted too often" >&2; exit 1; }}
    echo active > "$u/$3"; [ "$3" = postgresql.service ] && echo {polls} > "$u/.setup-polls" ;;
  show) prop="$4"; unit="$2"
    # As systemd 261: an unloaded unit reads not-found/inactive, exit 0.
    if [ "$3" = --property=LoadState,ActiveState ]; then
      if [ -f "$u/$unit" ]; then echo LoadState=loaded; echo "ActiveState=$(cat "$u/$unit")"
      else echo LoadState=not-found; echo ActiveState=inactive; fi; exit 0
    fi
    if [ "$unit" = setup.service ] && [ -f "$u/.setup-polls" ]; then
      n=$(cat "$u/.setup-polls")
      if [ "$n" != 0 ] && [ "$n" != forever ]; then [ "$prop" = --property=Job ] && echo $((n-1)) > "$u/.setup-polls"; [ "$prop" = --property=Job ] && echo 7 || echo activating; exit 0; fi
      if [ "$n" = forever ]; then [ "$prop" = --property=Job ] && echo 7 || echo activating; exit 0; fi
    fi
    [ "$prop" = --property=Job ] && echo "" || cat "$u/$unit" ;;
esac
exit 0"#,
                    units = p("units").display(),
                    log = log.display(),
                ),
            );
            // The preparation worker always reports "not ready".
            executable(
                &p("systemd-run"),
                &format!("echo \"systemd-run $*\" >> {}\nexit 1", log.display()),
            );
            executable(
                &p("harbor"),
                &format!(
                    r#"log={log}; shift 2; echo "harbor $*" >> "$log"
case "$1" in
  inhibit-startup) [ -f {inhibited} ] || echo '{{"token":"{TOKEN}"}}' > {inhibited}; echo '{{"status":"startup-inhibited","token":"{TOKEN}"}}' ;;
  fence-open) echo '{{"token":"{TOKEN}"}}' > {marker}; echo '{{"status":"prepared-offline","token":"{TOKEN}","restart_required":true}}' ;;
  release-startup) rm -f {inhibited}; echo '{{"status":"startup-released"}}' ;;
  inspect-fence) echo '{{"status":"ready","token":"{TOKEN}","system_identifier":"42"}}' ;;
  inspect-offline-fence) exit 1 ;;
  fence-close) rm -f {marker}; echo '{{"status":"closed-offline","token":"{TOKEN}"}}' ;;
  inspect-live) echo '{{"system_identifier":"42"}}' ;;
  inspect-recovery) echo '{{"status":"ready"}}' ;;
  *) echo '{{}}' ;;
esac"#,
                    log = log.display(),
                    marker = p("authority/writer-fence.json").display(),
                    inhibited = p("startup/inhibited.json").display(),
                ),
            );
            let maintenance = json!({
                "data_dir": p("data"), "state_dir": p("authority"),
                "recovery": {"system_identifier": "42", "require_writer_fence": true,
                    "snapshot_file": p("evidence/records.json")},
                "startup_inhibition": {"unit": "postgresql.service", "drop_in_root": p("control"),
                    "state_dir": p("startup")},
            });
            fs::write(p("maintenance.json"), maintenance.to_string()).unwrap();
            let fixture = Self { root };
            fixture.manifest("rollout.json", 2);
            fixture
        }

        fn p(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        fn manifest(&self, name: &str, settle: u64) -> PathBuf {
            let manifest = json!({
                "version": 2, "host": "primary", "system_identifier": "42",
                "harbor": self.p("harbor"), "systemctl": self.p("systemctl"),
                "systemd_run": self.p("systemd-run"),
                "maintenance": self.p("maintenance.json"), "preparation": self.p("preparation.json"),
                "socket_dir": "/run/postgresql", "port": 5432,
                "clients": ["a.service", "b.service"], "retired_clients": [],
                "setup_units": ["setup.service"], "receiver_units": [],
                "paused_publishers": ["pub.service"], "setup_settle_seconds": settle,
                "prepare": {"export_root": self.p("export"), "read_write_paths": [],
                    "required_mounts": [], "supplementary_groups": [], "provision_directories": []},
                "import_slot": self.p("import/receipt.json"),
                "executor": {"host": "executor", "user": "can", "command": ["/bin/true"]},
                "cutover": null,
            });
            let path = self.p(name);
            fs::write(&path, manifest.to_string()).unwrap();
            path
        }

        fn rollout(&self) -> Rollout {
            self.load(&self.p("rollout.json")).unwrap()
        }

        /// The fixture's journal root and installed manifest stand in for the
        /// fixed host paths.
        fn load(&self, path: &Path) -> Result<Rollout> {
            let mut rollout = Rollout::load(path)?;
            rollout.state = self.p("state");
            rollout.installed = self.p("installed.json");
            // A build sandbox's `/` need not be root-owned.
            rollout.anchor = self.root.path().to_owned();
            Ok(rollout)
        }

        fn unit(&self, unit: &str) -> String {
            fs::read_to_string(self.p("units").join(unit))
                .unwrap()
                .trim()
                .to_owned()
        }

        fn calls(&self) -> String {
            fs::read_to_string(self.p("calls.log")).unwrap()
        }
    }

    #[test]
    fn fence_waits_for_setup_units_then_abort_restores_the_primary_and_clients() {
        let f = Fixture::new(3);
        let status = f.rollout().run(Action::Fence).unwrap();
        assert_eq!(status["phase"], "fenced");
        assert_eq!(status["fence_token"], TOKEN);
        assert_eq!(f.unit("a.service"), "inactive");
        assert_eq!(f.unit("pub.service"), "inactive");
        assert!(f.p("state/paused").exists());
        assert!(f.p("control/pub.service.d").join(PAUSE_DROP_IN).exists());
        let calls = f.calls();
        let inspected = calls.find("harbor inspect-fence").unwrap();
        let polls = calls[..inspected]
            .matches("show setup.service --value --property=Job")
            .count();
        assert!(polls >= 4, "inspected after {polls} setup polls");
        assert!(
            calls.find("harbor fence-open").unwrap()
                > calls.find("stop -- postgresql.service").unwrap()
        );

        let status = f
            .rollout()
            .run(Action::Abort {
                adopt_fence_token: None,
            })
            .unwrap();
        assert_eq!(status["phase"], "aborted");
        assert_eq!(f.unit("a.service"), "active");
        assert_eq!(f.unit("b.service"), "active");
        assert_eq!(f.unit("postgresql.service"), "active");
        assert!(!f.p("state/paused").exists());
        let calls = f.calls();
        for step in [
            "harbor fence-close",
            "release-startup --token",
            "--phase closed",
            "harbor inspect-live",
        ] {
            assert!(calls.contains(step), "missing {step}");
        }
    }

    #[test]
    fn unsettled_setup_units_keep_the_fence_unverified() {
        let f = Fixture::new(0);
        fs::write(f.p("units/.setup-polls"), "forever").unwrap();
        f.manifest("rollout.json", 0);
        // The fake resets the counter on primary start; make it perpetual.
        let script = fs::read_to_string(f.p("systemctl")).unwrap().replace(
            "echo 0 > \"$u/.setup-polls\"",
            "echo forever > \"$u/.setup-polls\"",
        );
        executable(&f.p("systemctl"), script.trim_start_matches("#!/bin/sh\n"));
        let error = f.rollout().run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("did not settle"), "{error}");
        assert!(!f.calls().contains("harbor inspect-fence"));
        let journal = f.rollout().journal().unwrap().unwrap();
        assert_eq!(journal.phase, Phase::Idle);
        assert_eq!(journal.fence_token.as_deref(), Some(TOKEN));
    }

    #[test]
    fn fence_resumes_idempotently_and_steps_stay_bound_to_their_manifest() {
        let f = Fixture::new(0);
        f.rollout().run(Action::Fence).unwrap();
        fs::write(f.p("units/a.service"), "active").unwrap();
        assert_eq!(f.rollout().run(Action::Fence).unwrap()["phase"], "fenced");
        assert_eq!(f.unit("a.service"), "inactive");
        assert!(f.rollout().run(Action::Adopt).is_err());

        let other = f.load(&f.manifest("other.json", 5)).unwrap();
        let error = other.run(Action::Prepare).unwrap_err().to_string();
        assert!(error.contains("another manifest"), "{error}");
        // A different manifest of the same primary may still unwind the fence.
        assert_eq!(
            other
                .run(Action::Abort {
                    adopt_fence_token: None
                })
                .unwrap()["phase"],
            "aborted"
        );
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn activated_rollouts_refuse_abort_and_receipts_are_digest_bound() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        rollout.save(&journal).unwrap();
        let receipt = f.p("receipt.json");
        fs::write(&receipt, b"{\"status\":\"ready\"}").unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let error = rollout
            .run(Action::Import {
                receipt: receipt.clone(),
                sha256: "0".repeat(64),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!f.p("import/receipt.json").exists());

        journal.phase = Phase::Activated;
        rollout.save(&journal).unwrap();
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let error = rollout
            .run(Action::Abort {
                adopt_fence_token: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("activated"), "{error}");
        assert_eq!(f.unit("a.service"), "inactive");
        let error = rollout
            .run(Action::Thaw {
                acceptance: receipt,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("root-owned"), "{error}");
    }

    #[test]
    fn preparation_quiesces_transitional_receivers_and_may_write_its_export() {
        let f = Fixture::new(0);
        let path = f.p("rollout.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["receiver_units"] = json!(["r.service"]);
        fs::write(&path, manifest.to_string()).unwrap();
        fs::write(f.p("units/r.service"), "activating").unwrap();
        let rollout = f.rollout();
        let export = format!("{}", f.p("export").display());
        let writable = rollout
            .preparation_command(false)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .any(|arg| arg.starts_with("--property=ReadWritePaths=") && arg.contains(&export));
        assert!(writable, "export root must be writable by the worker");
        assert!(
            !rollout
                .run_preparation(&mut rollout.bound().unwrap(), false)
                .unwrap()
        );
        let calls = f.calls();
        assert!(calls.contains("systemctl stop -- r.service"), "{calls}");
        assert!(calls.contains("systemctl start -- r.service"), "{calls}");
    }

    #[test]
    fn interrupted_import_reuses_its_staged_receipt_but_never_replaces_another() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        rollout.save(&journal).unwrap();
        let bytes = br#"{"status":"ready"}"#;
        let receipt = f.p("receipt.json");
        fs::write(&receipt, bytes).unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(f.p("import")).unwrap();
        fs::write(f.p("import/receipt.json"), bytes).unwrap();
        fs::set_permissions(
            f.p("import/receipt.json"),
            fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        let import = || {
            rollout
                .run(Action::Import {
                    receipt: receipt.clone(),
                    sha256: codec::digest(bytes),
                })
                .unwrap_err()
                .to_string()
        };
        // The staged copy is reused, so the retry reaches admission.
        let error = import();
        assert!(error.contains("not admitted"), "{error}");
        fs::set_permissions(
            f.p("import/receipt.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(f.p("import/receipt.json"), br#"{"status":"other"}"#).unwrap();
        let error = import();
        assert!(error.contains("different receipt"), "{error}");
    }

    fn edit_manifest(f: &Fixture, edit: impl FnOnce(&mut Value)) -> Result<Rollout> {
        let path = f.p("rollout.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut manifest);
        fs::write(&path, manifest.to_string()).unwrap();
        f.load(&path)
    }

    #[test]
    fn load_rejects_shared_overlapping_or_mutable_rollout_inputs() {
        let f = Fixture::new(0);
        // The journal root is fixed and the identity drop is in-process, so a
        // manifest can neither relocate the journal nor name a runuser.
        for (field, value) in [
            ("state_dir", json!(f.p("state"))),
            ("runuser", json!("/run/wrappers/bin/runuser")),
            ("version", json!(1)),
            ("execution_lock", json!("switch.lock")),
        ] {
            assert!(
                edit_manifest(&f, |m| m[field] = value.clone()).is_err(),
                "{field}={value}"
            );
            f.manifest("rollout.json", 2);
        }
        for export in [
            json!("/"),
            json!(f.p("data")),
            json!(f.p("authority").join("x")),
        ] {
            let error = edit_manifest(&f, |m| m["prepare"]["export_root"] = export.clone())
                .err()
                .unwrap();
            assert!(error.to_string().contains("export"), "{export}: {error}");
            f.manifest("rollout.json", 2);
        }
        let error = edit_manifest(&f, |m| {
            m["cutover"] =
                json!({"checker": f.p("checker"), "contract": "/nix/store/x-c/manifest.json"})
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("store paths"), "{error}");
    }

    #[test]
    fn an_existing_shared_state_directory_is_refused_not_repermissioned() {
        let f = Fixture::new(0);
        fs::set_permissions(f.p("state"), fs::Permissions::from_mode(0o755)).unwrap();
        let error = f.rollout().run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("private directory"), "{error}");
        assert_eq!(fs::metadata(f.p("state")).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn receivers_are_restored_when_a_later_receiver_cannot_be_stopped() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["receiver_units"] = json!(["r1.service", "r2.service"])
        })
        .unwrap();
        fs::write(f.p("units/r1.service"), "active").unwrap();
        // A directory makes the fake systemctl's stop and show fail for r2.
        fs::create_dir(f.p("units/r2.service")).unwrap();
        assert!(
            rollout
                .run_preparation(&mut rollout.bound().unwrap(), false)
                .is_err()
        );
        assert!(
            f.calls().contains("systemctl start -- r1.service"),
            "{}",
            f.calls()
        );
        assert_eq!(f.unit("r1.service"), "active");
    }

    #[test]
    fn preparation_refuses_an_unrecorded_import_slot() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        fs::create_dir_all(f.p("import")).unwrap();
        fs::write(f.p("import/receipt.json"), br#"{"status":"ready"}"#).unwrap();
        fs::set_permissions(
            f.p("import/receipt.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let journal = rollout.journal().unwrap().unwrap();
        let error = rollout.import_slot(&journal).unwrap_err().to_string();
        assert!(error.contains("unrecorded"), "{error}");
    }

    #[test]
    fn untrusted_state_directory_ancestry_is_refused() {
        let f = Fixture::new(0);
        fs::create_dir(f.p("shared")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o777)).unwrap();
        let mut rollout = f.rollout();
        rollout.state = f.p("shared/rollout");
        let error = rollout.run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("ancestry"), "{error}");
        assert!(!f.p("shared/rollout").exists());
    }

    #[test]
    fn missing_journal_components_are_never_created_below_a_writable_directory() {
        let f = Fixture::new(0);
        fs::create_dir(f.p("sticky")).unwrap();
        fs::set_permissions(f.p("sticky"), fs::Permissions::from_mode(0o1777)).unwrap();
        let mut rollout = f.rollout();
        // A sticky directory is a trusted ancestor, but a name created below it
        // can be raced, so the coordinator never creates one there.
        rollout.state = f.p("sticky/missing/rollout");
        let error = rollout.state_dir().unwrap_err().to_string();
        assert!(error.contains("ancestry"), "{error}");
        assert!(!f.p("sticky/missing").exists());
        // Created intermediates stay traversable for service users; the leaf is private.
        rollout.state = f.p("fresh/journal");
        rollout.state_dir().unwrap();
        assert_eq!(fs::metadata(f.p("fresh")).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(
            fs::metadata(f.p("fresh/journal")).unwrap().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn every_quiesced_receiver_is_restarted_even_if_one_restart_fails() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["receiver_units"] = json!(["r1.service", "r2.service"])
        })
        .unwrap();
        fs::write(f.p("units/r1.service"), "active").unwrap();
        fs::write(f.p("units/r2.service"), "active").unwrap();
        // Make only r1's restart fail; r2 must still be started afterwards.
        let systemctl = fs::read_to_string(f.p("systemctl")).unwrap();
        fs::write(
            f.p("systemctl"),
            systemctl.replacen(
                "case \"$1\" in",
                "[ \"$1 $3\" = \"start r1.service\" ] && exit 1\ncase \"$1\" in",
                1,
            ),
        )
        .unwrap();
        let mut journal = rollout.bound().unwrap();
        assert!(rollout.run_preparation(&mut journal, false).is_err());
        assert_eq!(f.unit("r2.service"), "active");
        // The failed restart stays recorded and is retried by the next run.
        assert_eq!(journal.pending_receivers, ["r1.service"]);
        fs::write(f.p("systemctl"), systemctl).unwrap();
        rollout.restart_receivers(&mut journal).unwrap();
        assert!(journal.pending_receivers.is_empty());
        assert_eq!(f.unit("r1.service"), "active");
    }

    #[test]
    fn abort_unwinds_an_acquisition_interrupted_before_fence_open() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        let mut journal = rollout.bound().unwrap();
        journal.startup_token = Some(TOKEN.into());
        journal.clients = vec!["a.service".into()];
        rollout.save(&journal).unwrap();
        fs::write(
            f.p("startup/inhibited.json"),
            json!({ "token": TOKEN }).to_string(),
        )
        .unwrap();
        fs::write(f.p("units/a.service"), "inactive").unwrap();
        rollout
            .run(Action::Abort {
                adopt_fence_token: None,
            })
            .unwrap();
        let calls = f.calls();
        assert!(calls.contains("harbor fence-open"), "{calls}");
        assert!(calls.contains("harbor fence-close"), "{calls}");
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn the_state_directory_is_never_created_below_an_absent_mount() {
        let f = Fixture::new(0);
        fs::create_dir(f.p("mnt")).unwrap();
        let mut rollout = edit_manifest(&f, |m| {
            m["prepare"]["required_mounts"] = json!([f.p("mnt")])
        })
        .unwrap();
        rollout.state = f.p("mnt/rollout");
        let error = rollout.run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("required mount"), "{error}");
        assert!(!f.p("mnt/rollout").exists());
    }

    #[test]
    fn next_hands_off_external_steps_with_exact_commands() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        assert_eq!(rollout.run(Action::Next).unwrap()["phase"], "fenced");
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        journal.export = Some(f.p("export/backup-1"));
        journal.export_sha256 = Some("a".repeat(64));
        rollout.save(&journal).unwrap();
        let status = rollout.run(Action::Next).unwrap();
        assert_eq!(status["phase"], "prepared");
        let next = status["next"].as_str().unwrap();
        assert!(
            next.contains("rollout certify") && next.contains(&"a".repeat(64)),
            "{next}"
        );
        assert!(next.contains("on executor as can"), "{next}");
        // The executor needs the manifest's closure (Harbor, certify argv) too.
        let config = f.p("rollout.json");
        assert!(
            next.contains(&format!(
                "nix copy --to ssh-ng://executor {}",
                config.display()
            )) && next.contains(&format!("{} --config", f.p("harbor").display())),
            "{next}"
        );
    }

    fn fail_start(f: &Fixture, unit: &str) -> String {
        let systemctl = fs::read_to_string(f.p("systemctl")).unwrap();
        fs::write(
            f.p("systemctl"),
            systemctl.replacen(
                "case \"$1\" in",
                &format!("[ \"$1 $3\" = \"start {unit}\" ] && exit 1\ncase \"$1\" in"),
                1,
            ),
        )
        .unwrap();
        systemctl
    }

    const NO_ADOPTION: Action = Action::Abort {
        adopt_fence_token: None,
    };

    #[test]
    fn a_fence_this_rollout_did_not_open_is_refused_not_adopted() {
        let f = Fixture::new(0);
        let other = "f".repeat(32);
        let marker = f.p("authority/writer-fence.json");
        fs::write(&marker, json!({ "token": other }).to_string()).unwrap();
        let error = f.rollout().run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("did not open"), "{error}");
        // The refusal precedes every side effect, so production is untouched
        // and nothing is left for `abort` to unwind.
        let untouched = |f: &Fixture| {
            let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
            assert!(
                !calls.contains("systemctl stop")
                    && !calls.contains("inhibit-startup")
                    && !calls.contains("daemon-reload")
                    && !calls.contains("harbor fence-open"),
                "{calls}"
            );
            for unit in ["postgresql.service", "a.service", "pub.service"] {
                assert_eq!(f.unit(unit), "active", "{unit}");
            }
            assert!(f.rollout().journal().unwrap().is_none());
        };
        untouched(&f);
        // An interrupted acquisition's selector is someone's fence too.
        fs::remove_file(&marker).unwrap();
        fs::write(
            f.p("data/postgresql.auto.conf"),
            format!("port = 5432\n# harbor-db-writer-fence={other}\n"),
        )
        .unwrap();
        let error = f.rollout().run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains(&other), "{error}");
        untouched(&f);
        // A fence this rollout recorded opening (then crashed before recording
        // its token) is resumed, not refused.
        fs::remove_file(f.p("data/postgresql.auto.conf")).unwrap();
        let rollout = f.rollout();
        let mut journal = rollout.bound().unwrap();
        journal.fence_opening = true;
        rollout.save(&journal).unwrap();
        fs::write(&marker, json!({ "token": TOKEN }).to_string()).unwrap();
        assert_eq!(rollout.run(Action::Fence).unwrap()["fence_token"], TOKEN);
    }

    #[test]
    fn mutating_steps_hold_the_shared_execution_lock_but_status_does_not() {
        let f = Fixture::new(0);
        let lock = f.p("switch.lock");
        fs::write(&lock, "").unwrap();
        let rollout = edit_manifest(&f, |m| m["execution_lock"] = json!(lock)).unwrap();
        let held = durable::lock(&lock, false, false).unwrap();
        let error = rollout.run(Action::Fence).unwrap_err().to_string();
        assert!(
            error.contains("execution lock") && error.contains(&std::process::id().to_string()),
            "{error}"
        );
        assert!(!f.p("calls.log").exists(), "no step ran without the lock");
        assert_eq!(rollout.status().unwrap()["phase"], "idle");
        drop(held);
        assert_eq!(rollout.run(Action::Fence).unwrap()["phase"], "fenced");
    }

    #[test]
    fn an_active_preparation_worker_blocks_preparation_receivers_and_the_pause() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        let unit = format!("--unit={PREPARE_UNIT}");
        assert!(
            rollout
                .preparation_command(false)
                .get_args()
                .any(|arg| arg == unit.as_str())
        );
        rollout.run(Action::Fence).unwrap();
        // An orphaned worker from an interrupted run is still preparing.
        fs::write(f.p("units").join(PREPARE_UNIT), "active").unwrap();
        fs::write(f.p("units/r.service"), "inactive").unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.pending_receivers = vec!["r.service".into()];
        rollout.save(&journal).unwrap();
        assert_eq!(
            rollout.status().unwrap()["preparation_worker"],
            PREPARE_UNIT
        );
        for error in [
            rollout.run(Action::Prepare).unwrap_err(),
            rollout.run(NO_ADOPTION).unwrap_err(),
            rollout.lift_pause(&mut journal).unwrap_err(),
        ] {
            assert!(error.to_string().contains(PREPARE_UNIT), "{error}");
        }
        assert!(!f.calls().contains("systemd-run"));
        assert_eq!(f.unit("r.service"), "inactive");
        assert!(f.p("state/paused").exists());
    }

    #[test]
    fn an_interrupted_preparation_stops_its_worker_and_restores_receivers() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        fs::write(f.p("units/r.service"), "active").unwrap();
        // The worker runs until its unit is stopped.
        let unit = f.p("units").join(PREPARE_UNIT);
        executable(
            &f.p("systemd-run"),
            &format!(
                "while read -r key value; do [ \"$key\" = SigBlk: ] && echo \"$value\" > {blk}; done < /proc/$$/status\necho active > {u}\nwhile [ \"$(cat {u})\" = active ]; do sleep 0.05; done\nexit 1",
                u = unit.display(),
                blk = f.p("worker-mask").display()
            ),
        );
        // A terminal interrupt reaches the coordinator thread while it waits.
        let coordinator = unsafe { libc::pthread_self() };
        let interrupter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            unsafe { libc::pthread_kill(coordinator, libc::SIGINT) };
        });
        let mut journal = rollout.bound().unwrap();
        let error = rollout.run_preparation(&mut journal, false);
        interrupter.join().unwrap();
        let error = error.unwrap_err().to_string();
        assert!(error.contains("interrupted"), "{error}");
        assert_eq!(f.unit(PREPARE_UNIT), "inactive");
        assert_eq!(f.unit("r.service"), "active");
        assert!(journal.pending_receivers.is_empty());
        // The worker client does not inherit the coordinator's deferral.
        let mask = fs::read_to_string(f.p("worker-mask")).unwrap();
        let mask =
            u64::from_str_radix(mask.trim().trim_start_matches("SigBlk:").trim(), 16).unwrap();
        assert_eq!(mask & 0x4003, 0, "{mask:x}");
    }

    #[test]
    fn subprocesses_are_bounded_and_report_their_diagnostics() {
        let started = Instant::now();
        let error = spawn_bounded(
            vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            None,
            Duration::from_millis(200),
        )
        .unwrap_err()
        .to_string();
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        assert!(error.contains("sleep 5"), "{error}");
        let output = spawn_bounded(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "pwd; echo nope >&2; exit 3".into(),
            ],
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.stdout, b"/\n");
        let error = checked(output).unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn an_unwinding_retry_never_restarts_the_reopened_primary() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        // A client fails to start after the fence is already closed.
        let systemctl = fail_start(&f, "a.service");
        assert!(rollout.run(NO_ADOPTION).is_err());
        assert!(rollout.journal().unwrap().unwrap().reopened);
        let error = rollout.run(Action::Prepare).unwrap_err().to_string();
        assert!(error.contains("unwinding"), "{error}");
        fs::write(f.p("systemctl"), systemctl).unwrap();
        let stops = |calls: &str| calls.matches("stop -- postgresql.service").count();
        let before = stops(&f.calls());
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        let calls = f.calls();
        assert_eq!(stops(&calls), before, "{calls}");
        assert_eq!(calls.matches("harbor fence-close").count(), 1);
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn activated_and_thaw_require_the_running_generation_to_carry_the_manifest() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Adopted;
        rollout.save(&journal).unwrap();
        let error = rollout.run(Action::Activated).unwrap_err().to_string();
        assert!(error.contains("running generation"), "{error}");
        std::os::unix::fs::symlink(f.manifest("other.json", 5), f.p("installed.json")).unwrap();
        let error = rollout.run(Action::Activated).unwrap_err().to_string();
        assert!(error.contains("running generation"), "{error}");
        fs::remove_file(f.p("installed.json")).unwrap();
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        assert_eq!(
            rollout.run(Action::Activated).unwrap()["phase"],
            "activated"
        );
        fs::remove_file(f.p("installed.json")).unwrap();
        std::os::unix::fs::symlink(f.p("other.json"), f.p("installed.json")).unwrap();
        let error = rollout
            .run(Action::Thaw {
                acceptance: f.p("acceptance.json"),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("running generation"), "{error}");
    }

    #[test]
    fn a_lost_journal_is_aborted_only_by_explicitly_adopting_the_live_fence() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        rollout.run(Action::Fence).unwrap();
        fs::write(f.p("units/r.service"), "inactive").unwrap();
        fs::remove_file(f.p("state/rollout.json")).unwrap();
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(
            error.contains(&format!("--adopt-fence-token {TOKEN}"))
                && error.contains("inspect-fence"),
            "{error}"
        );
        let error = rollout
            .run(Action::Abort {
                adopt_fence_token: Some("f".repeat(32)),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("differs"), "{error}");
        assert!(!f.calls().contains("fence-close"));
        let status = rollout
            .run(Action::Abort {
                adopt_fence_token: Some(TOKEN.into()),
            })
            .unwrap();
        assert_eq!(status["phase"], "aborted");
        assert!(status["next"].as_str().unwrap().contains("writer clients"));
        assert!(
            f.calls()
                .contains(&format!("harbor fence-close --token {TOKEN}"))
        );
        assert_eq!(f.unit("postgresql.service"), "active");
        assert_eq!(f.unit("r.service"), "active");
        assert!(!f.p("state/paused").exists());
        // The pause stopped this publisher; the lost journal cannot say so.
        assert_eq!(f.unit("pub.service"), "active");
    }

    #[test]
    fn a_never_loaded_or_collected_worker_is_idle_whatever_its_state_reads() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        assert!(!rollout.preparing().unwrap());
        let systemctl = fs::read_to_string(f.p("systemctl")).unwrap();
        fs::write(
            f.p("systemctl"),
            systemctl.replace(
                "echo LoadState=not-found; echo ActiveState=inactive",
                "echo LoadState=not-found",
            ),
        )
        .unwrap();
        assert!(!rollout.preparing().unwrap());
        assert!(
            rollout
                .status()
                .unwrap()
                .get("preparation_worker")
                .is_none()
        );
        let unit = f.p("units").join(PREPARE_UNIT);
        fs::write(&unit, "activating").unwrap();
        assert!(rollout.preparing().unwrap());
        fs::write(&unit, "failed").unwrap();
        assert!(!rollout.preparing().unwrap());
    }

    #[test]
    fn sandbox_and_provisioned_paths_never_resolve_through_a_swappable_component() {
        let f = Fixture::new(0);
        let prepare = |edit: &dyn Fn(&mut Value)| {
            let rollout = edit_manifest(&f, |m| edit(m)).unwrap();
            let error = rollout
                .run_preparation(&mut rollout.bound().unwrap(), false)
                .unwrap_err()
                .to_string();
            f.manifest("rollout.json", 2);
            error
        };
        // A symlinked export root would make systemd grant the target write access.
        std::os::unix::fs::symlink(f.p("data"), f.p("export")).unwrap();
        let error = prepare(&|_| {});
        assert!(error.contains("untrusted ancestry"), "{error}");
        fs::remove_file(f.p("export")).unwrap();
        // So would a writable path whose parent another user can modify.
        fs::create_dir_all(f.p("shared/backups")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o777)).unwrap();
        let error = prepare(&|m| m["prepare"]["read_write_paths"] = json!([f.p("shared/backups")]));
        assert!(error.contains("untrusted ancestry"), "{error}");
        // Provisioning never follows (nor re-permissions) a symlink.
        std::os::unix::fs::symlink(f.p("data"), f.p("provisioned")).unwrap();
        let mode = fs::metadata(f.p("data")).unwrap().mode();
        let error = prepare(&|m| {
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("provisioned"), "group": "postgres", "mode": 448}])
        });
        assert!(error.contains("untrusted ancestry"), "{error}");
        assert_eq!(fs::metadata(f.p("data")).unwrap().mode(), mode);
        assert!(!f.p("calls.log").exists() || !f.calls().contains("systemd-run"));
    }

    #[test]
    fn deliberate_starts_clear_the_systemd_start_limit() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        rollout.run(Action::Fence).unwrap();
        fs::write(f.p("units/r.service"), "inactive").unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.pending_receivers = vec!["r.service".into()];
        rollout.save(&journal).unwrap();
        // Earlier abort/retry cycles spent every unit's start burst.
        let units = ["postgresql.service", "a.service", "b.service", "r.service"];
        for unit in units {
            fs::write(f.p("units").join(format!(".starts-{unit}")), "5").unwrap();
        }
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        for unit in units {
            assert_eq!(f.unit(unit), "active", "{unit}");
        }
    }

    #[test]
    fn a_finished_abort_is_archived_so_the_candidate_can_start_again() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        let aborted = rollout.run(NO_ADOPTION).unwrap();
        assert!(
            aborted["next"].as_str().unwrap().contains("archives"),
            "{aborted}"
        );
        // An abort whose fence is held again is not fully unwound.
        let marker = f.p("authority/writer-fence.json");
        fs::write(&marker, json!({ "token": "f".repeat(32) }).to_string()).unwrap();
        let error = rollout.run(Action::Fence).unwrap_err().to_string();
        assert!(error.contains("not fully unwound"), "{error}");
        assert!(rollout.archives().unwrap().is_empty());
        fs::remove_file(&marker).unwrap();
        assert_eq!(rollout.run(Action::Fence).unwrap()["phase"], "fenced");
        let archives = rollout.archives().unwrap();
        assert_eq!(archives.len(), 1, "{archives:?}");
        let archived = durable::read_json(&f.p("state").join(&archives[0])).unwrap();
        assert_eq!(archived["phase"], "aborted");
        assert_eq!(rollout.status().unwrap()["archived"], json!(archives));
    }

    #[test]
    fn paused_publishers_that_were_running_are_restarted_after_the_pause() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["paused_publishers"] = json!(["pub.service", "idle-pub.service"])
        })
        .unwrap();
        fs::write(f.p("units/idle-pub.service"), "inactive").unwrap();
        rollout.run(Action::Fence).unwrap();
        assert_eq!(f.unit("pub.service"), "inactive");
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.stopped_publishers, ["pub.service"]);
        rollout.run(NO_ADOPTION).unwrap();
        assert_eq!(f.unit("pub.service"), "active");
        // A publisher that was idle stays for its own trigger.
        assert_eq!(f.unit("idle-pub.service"), "inactive");
        assert!(
            rollout
                .journal()
                .unwrap()
                .unwrap()
                .stopped_publishers
                .is_empty()
        );
        assert!(!f.p("state/paused").exists());
    }

    #[test]
    fn systemd_property_values_never_split_into_extra_entries() {
        let f = Fixture::new(0);
        for edit in [
            json!({"prepare": {"read_write_paths": ["/srv/backups /srv/postgres/18"]}}),
            json!({"prepare": {"export_root": "/srv/harbor-db/ex port"}}),
            json!({"prepare": {"required_mounts": ["/srv\tdata"]}}),
            json!({"prepare": {"supplementary_groups": ["users wheel"]}}),
            json!({"prepare": {"provision_directories": [{"path": "/srv/a%h", "group": "postgres", "mode": 448}]}}),
            json!({"import_slot": "/srv/import/off:host.json"}),
            json!({"prepare": {"read_write_paths": ["/srv/\"quoted\""]}}),
        ] {
            let error = edit_manifest(&f, |m| {
                for (key, value) in edit.as_object().unwrap() {
                    match value.as_object() {
                        Some(fields) => fields.iter().for_each(|(k, v)| m[key][k] = v.clone()),
                        None => m[key] = value.clone(),
                    }
                }
            })
            .err()
            .unwrap()
            .to_string();
            assert!(error.contains("systemd property"), "{edit}: {error}");
            f.manifest("rollout.json", 2);
        }
        assert!(
            edit_manifest(&f, |m| m["prepare"]["read_write_paths"] =
                json!(["/srv/backups"]))
            .is_ok()
        );
    }

    #[test]
    fn preparation_never_provisions_below_an_absent_mount() {
        let f = Fixture::new(0);
        fs::create_dir(f.p("mnt")).unwrap();
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["required_mounts"] = json!([f.p("mnt")]);
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("mnt/export"), "group": "postgres", "mode": 488}]);
        })
        .unwrap();
        let error = rollout
            .run_preparation(&mut rollout.bound().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("required mount"), "{error}");
        assert!(!f.p("mnt/export").exists());
        assert!(!f.calls().contains("systemd-run"));
    }

    #[test]
    fn activation_is_refused_between_fence_and_adoption() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        assert!(rollout.activation_check().is_ok(), "no rollout");
        rollout.run(Action::Fence).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        for phase in [Phase::Fenced, Phase::Prepared, Phase::Imported] {
            journal.phase = phase;
            rollout.save(&journal).unwrap();
            let error = rollout.activation_check().unwrap_err().to_string();
            assert!(
                error.contains("between fence and adoption"),
                "{phase:?}: {error}"
            );
        }
        journal.phase = Phase::Idle;
        rollout.save(&journal).unwrap();
        assert!(rollout.activation_check().is_err(), "interrupted fence");
        journal.phase = Phase::Adopted;
        rollout.save(&journal).unwrap();
        assert!(rollout.activation_check().is_ok());
        let other = f.load(&f.manifest("other.json", 5)).unwrap();
        let error = other.activation_check().unwrap_err().to_string();
        assert!(error.contains("another manifest"), "{error}");
        for phase in [Phase::Activated, Phase::Thawed, Phase::Aborted] {
            journal.phase = phase;
            rollout.save(&journal).unwrap();
            assert!(other.activation_check().is_ok(), "{phase:?}");
        }
    }

    #[test]
    fn pre_fence_completion_never_adopts_another_coordinators_fence() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        // Interrupted between inhibition and fence-open, then another
        // coordinator opened its own fence.
        let mut journal = rollout.bound().unwrap();
        journal.startup_token = Some(TOKEN.into());
        rollout.save(&journal).unwrap();
        let inhibited = f.p("startup/inhibited.json");
        fs::write(&inhibited, json!({ "token": TOKEN }).to_string()).unwrap();
        fs::write(f.p("units/postgresql.service"), "inactive").unwrap();
        let other = "f".repeat(32);
        let marker = f.p("authority/writer-fence.json");
        fs::write(&marker, json!({ "token": other }).to_string()).unwrap();
        // Harbor releases startup only at a fence boundary: name both tokens.
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(
            error.contains(TOKEN) && error.contains(&other) && error.contains("then rerun"),
            "{error}"
        );
        let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
        assert!(
            !calls.contains("harbor fence-open") && !calls.contains("systemctl stop"),
            "{calls}"
        );
        // Once the other coordinator released the shared inhibition at its own
        // boundary, the retry restarts the primary without adopting its fence.
        fs::remove_file(&inhibited).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("postgresql.service"), "active");
        let calls = f.calls();
        assert!(
            !calls.contains("harbor fence-open") && !calls.contains("harbor inspect-live"),
            "{calls}"
        );
    }

    #[test]
    fn a_thawed_journal_is_archived_so_a_new_generation_can_roll_out() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(Action::Fence).unwrap();
        // The thaw closed the fence and reopened under the old manifest.
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Thawed;
        journal.reopened = true;
        rollout.save(&journal).unwrap();
        fs::remove_file(f.p("authority/writer-fence.json")).unwrap();
        let next = f.load(&f.manifest("next.json", 5)).unwrap();
        let status = next.run(Action::Fence).unwrap();
        assert_eq!(status["phase"], "fenced");
        assert_eq!(status["manifest_sha256"], next.digest);
        let archives = next.archives().unwrap();
        assert!(
            archives.len() == 1 && archives[0].ends_with(".thawed.json"),
            "{archives:?}"
        );
        let archived = durable::read_json(&f.p("state").join(&archives[0])).unwrap();
        assert_eq!(archived["manifest_sha256"], rollout.digest);
    }

    /// Steps print exactly one status document, so worker and executor output
    /// must reach stderr. Re-executes this test to observe the real descriptors.
    #[test]
    fn worker_and_executor_output_never_reaches_step_stdout() {
        const CHILD: &str = "HARBOR_DB_ROLLOUT_STDOUT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let f = Fixture::new(0);
            executable(&f.p("systemd-run"), "echo WORKER-CHATTER; exit 1");
            executable(&f.p("certify"), "echo EXECUTOR-CHATTER");
            let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
            let user = process::current_username().unwrap();
            let rollout =
                edit_manifest(
                    &f,
                    |m| {
                        m["executor"] =
                            json!({"host": host.trim(), "user": user, "command": [f.p("certify")]})
                    },
                )
                .unwrap();
            assert!(
                !rollout
                    .run_preparation(&mut rollout.bound().unwrap(), false)
                    .unwrap()
            );
            if !root() {
                rollout
                    .certify(&f.p("export"), &"a".repeat(64), &f.p("workspace"))
                    .unwrap();
            }
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::postgres_rollout::tests::worker_and_executor_output_never_reaches_step_stdout",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}{stderr}");
        let mut chatter = vec!["WORKER-CHATTER"];
        if !root() {
            chatter.push("EXECUTOR-CHATTER");
        }
        for line in chatter {
            assert!(
                stderr.contains(line) && !stdout.contains(line),
                "{line}: {stdout}"
            );
        }
    }

    #[test]
    fn an_unreadable_journal_is_an_error_not_an_idle_rollout() {
        if euid() == 0 {
            return;
        }
        let f = Fixture::new(0);
        f.rollout().run(Action::Fence).unwrap();
        fs::set_permissions(f.p("state"), fs::Permissions::from_mode(0o000)).unwrap();
        let status = f.rollout().status();
        fs::set_permissions(f.p("state"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(status.is_err());
    }
}
