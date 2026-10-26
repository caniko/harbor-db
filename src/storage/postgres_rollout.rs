//! First-rollout coordinator for a guarded PostgreSQL primary. It only sequences
//! the existing Harbor verbs (startup inhibition, writer fence, preparation,
//! adoption, thaw) behind one resumable journal; every path, unit and executor
//! comes from the immutable rollout manifest.
use super::{Result, codec, durable, invalid, process, writer_fence};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PAUSE_DROP_IN: &str = "zz-harbor-db-rollout-pause.conf";
/// Harbor's control records and the acceptance (receipts take recovery's
/// own, larger bound).
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
/// Record snapshots carry per-table records; still bounded.
const MAX_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;
/// The journal root is fixed, so every manifest of a primary (including a
/// superseded one) finds the same journal and no manifest can relocate it.
const STATE_ROOT: &str = "/var/lib/harbor-db/postgresql-rollout";
/// The manifest the running generation carries.
const INSTALLED: &str = "/etc/harbor-db/postgresql-rollout.json";
/// The same for the system profile (the next boot's default) and the
/// generation the host booted.
const PROFILE: &str = "/nix/var/nix/profiles/system/etc/harbor-db/postgresql-rollout.json";
const BOOTED: &str = "/run/booted-system/etc/harbor-db/postgresql-rollout.json";
const GC_ROOTS: &str = "/nix/var/nix/gcroots";
const GC_ROOT: &str = "harbor-db-postgresql-rollout";
/// A staged receipt the finished rollout retired from the import slot.
const RETIRED_RECEIPT: &str = "receipt.retired.json";
/// One deterministic worker: a retry finds an orphaned preparation by name.
const PREPARE_UNIT: &str = "harbor-db-rollout-prepare.service";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(600);
const HARBOR_TIMEOUT: Duration = Duration::from_secs(1800);
// ponytail: fixed one-day preparation bound; make it a manifest field if a
// cluster ever needs longer.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(86400);
const BUSY: &str = "a preparation worker (harbor-db-rollout-prepare.service) is still running; \
    wait for it (journalctl -u harbor-db-rollout-prepare.service) or stop it, then rerun";
/// The executor transport hook's deterministic unit (see `transport`).
const TRANSPORT_UNIT: &str = "harbor-db-rollout-transport.service";
const TRANSPORT_BUSY: &str = "an executor transport hook (harbor-db-rollout-transport.service) is \
    still running; wait for it (journalctl -u harbor-db-rollout-transport.service) or stop it, then rerun";

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
    /// Optional hand-off hook `rollout next` runs from `prepared` (see
    /// `postgres_rollout_transport.rs`); rendered only when set, so an older
    /// coordinator can still load a manifest without one.
    #[serde(default)]
    pub transport: Option<PathBuf>,
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

/// The journal's schema rule: new fields must be optional and additive and are
/// not written while empty, and unknown fields are ignored, so a rollback
/// generation can still read (and abort) a newer coordinator's journal. Only an
/// incompatible change bumps `version`; `version`, `system_identifier` and
/// `manifest_sha256` bind a journal, not field strictness (it is root-only).
#[derive(Deserialize, Serialize)]
pub struct Journal {
    pub version: u32,
    pub manifest_sha256: String,
    pub system_identifier: String,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_sha256: Option<String>,
    /// Receivers quiesced for preparation that still need a restart; retained
    /// so a failed or interrupted cleanup is retried, never forgotten.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_receivers: Vec<String>,
    /// Publishers that were running when paused; restarted after the pause.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped_publishers: Vec<String>,
    /// Recorded clients the unwind still has to restart: each leaves once
    /// started, so a retry never restarts a finished oneshot client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_clients: Vec<String>,
    /// Units to restore that the running generation does not load (removed
    /// by the candidate, or only it brings them): skipped, not retried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unloaded: Vec<String>,
    /// Recorded before `inhibit-startup`; with no token yet, the held
    /// inhibition is this rollout's (Harbor's inhibition is shared and
    /// idempotent, so the call itself would have returned that token).
    #[serde(default, skip_serializing_if = "is_false")]
    pub inhibiting: bool,
    /// Recorded once no foreign fence was found and before `fence-open`; with
    /// no token yet, a live fence is adopted only explicitly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fence_opening: bool,
    /// The fence is closed and the normal primary verified; an unwinding retry
    /// only returns clients, receivers and publishers.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reopened: bool,
    /// The manifest that opened the journal: the only one that may step or
    /// unwind it (kept alive by a GC root until the journal is archived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening_config: Option<PathBuf>,
    /// The executor transport's verified receipt, recorded before import stages it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transported: Option<transport::Transported>,
}

fn is_false(value: &bool) -> bool {
    !value
}

pub enum Action {
    Status,
    /// Pre-switch guard of a rollout-enabled generation.
    ActivationCheck,
    Next {
        adopt_fence_token: Option<String>,
    },
    Fence {
        /// Adopt the live fence after an opening interrupted before its token was recorded.
        adopt_fence_token: Option<String>,
    },
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
    /// Write the deployment acceptance thaw checks.
    Accept,
    Thaw {
        acceptance: PathBuf,
    },
    /// Archive a finished journal (and drop its GC root) without fencing.
    Archive,
    Abort {
        /// Explicitly adopt Harbor's live fence when the journal is lost.
        adopt_fence_token: Option<String>,
        /// Explicitly adopt Harbor's held startup inhibition when the journal
        /// is lost and no fence is live.
        adopt_startup_token: Option<String>,
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
    /// Owner of provisioned directories (`postgres` and each declared group on
    /// hosts; tests cannot chown to them).
    service_owner: Option<(u32, u32)>,
    /// Where the open journal's opening manifest is rooted against GC.
    gcroots: PathBuf,
    /// The manifest the system profile (the next boot's default) carries.
    profile: PathBuf,
    /// The manifest the booted generation carries.
    booted: PathBuf,
    /// Resolves a group name (`getgrnam` on hosts; tests stand in for
    /// service groups a build sandbox lacks).
    group: fn(&str) -> Result<u32>,
}

fn root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn operate(config: &Path, action: Action) -> Result<Value> {
    Rollout::load(&durable::immutable_config_path(config)?)?.dispatch(action)
}

/// Refuse activating a generation that would strand an open rollout: one
/// between fence and adoption, with an interrupted fence or unfinished abort,
/// or adopted or activated under a manifest other than `digest` (with no
/// manifest, as for a generation without the coordinator, any). Only a thawed,
/// fully aborted or never-started rollout lets another generation in.
fn stranding(journal: &Journal, digest: Option<&str>) -> Result<()> {
    let started = journal.startup_token.is_some()
        || journal.fence_token.is_some()
        || journal.inhibiting
        || journal.fence_opening;
    let opener = journal.opening_config.as_deref().map_or_else(
        || "its opening manifest".into(),
        |path| path.display().to_string(),
    );
    let reason = match journal.phase {
        Phase::Fenced | Phase::Prepared | Phase::Imported => {
            "the rollout is between fence and adoption; finish it or `rollout abort`".to_owned()
        }
        Phase::Idle if started => {
            "an interrupted rollout fence is pending; rerun `rollout fence` or `rollout abort`"
                .into()
        }
        Phase::Aborted if !journal.reopened => {
            "an abort is still unwinding; rerun `rollout abort`".into()
        }
        // A generation without the coordinator (`digest` None) only rolls
        // back: abort unwinds an adopted or activated candidate once it is
        // no longer installed, so only another rollout's manifest, or any
        // other generation mid-unwind, would strand it.
        Phase::Adopted | Phase::Activated
            if digest.is_some_and(|digest| digest != journal.manifest_sha256)
                || (journal.reopened && digest != Some(journal.manifest_sha256.as_str())) =>
        {
            format!(
                "the rollout was adopted under {opener}; thaw that candidate, or roll back and `rollout abort`, first"
            )
        }
        _ => return Ok(()),
    };
    Err(invalid(format!(
        "{reason} before activating another generation"
    )))
}

/// Pre-switch guard of a generation without the rollout coordinator: it reads
/// only the fixed journal, since no rollout manifest is installed.
pub fn activation_guard() -> Result<Value> {
    guard_at(Path::new(STATE_ROOT))
}

fn guard_at(state: &Path) -> Result<Value> {
    match load_journal(&state.join("rollout.json"))? {
        Some(journal) => {
            stranding(&journal, None)?;
            Ok(json!({"status": "clear", "phase": journal.phase}))
        }
        None => Ok(json!({"status": "clear", "phase": "idle"})),
    }
}

/// The journal at `path`, if present; an unreadable one is an error.
fn load_journal(path: &Path) -> Result<Option<Journal>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let journal: Journal = serde_json::from_value(durable::read_json(path)?)?;
    if journal.version != 1 {
        return Err(invalid("unsupported rollout journal version"));
    }
    Ok(Some(journal))
}

fn valid_unit(unit: &str) -> bool {
    unit.ends_with(".service")
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@.:-".contains(&b))
}

/// Whether a unit in ActiveState `state` is running or on its way somewhere
/// (active, activating, reloading, deactivating, or a state a newer systemd
/// adds): one the rollout records before stopping it, or before the primary's
/// stop takes it down, and restores afterwards. Only a settled unit (inactive
/// or failed) is not; the one rule for clients, receivers, publishers, hard
/// dependents and the worker.
fn restorable(state: &str) -> bool {
    !matches!(state, "inactive" | "failed")
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

/// Whether `identity` may write the directory `metadata` describes (read
/// through its no-follow descriptor). POSIX mode bits only: ACLs are not
/// considered.
fn require_writable(
    metadata: &fs::Metadata,
    path: &Path,
    (uid, groups): &(u32, Vec<u32>),
) -> Result<()> {
    let write = if metadata.uid() == *uid {
        0o200
    } else if groups.contains(&metadata.gid()) {
        0o020
    } else {
        0o002
    };
    if metadata.mode() & write == 0 {
        return Err(invalid(format!(
            "{} is not writable by postgres, so the preparation worker would fail after the fence; grant it write permission first",
            path.display()
        )));
    }
    Ok(())
}

/// A local executable a step runs: a regular file of the immutable store
/// reached without a mutable alias (as the store reference requires), and
/// executable.
fn store_executable(path: &Path) -> bool {
    durable::immutable_store_reference(path)
        .and_then(|resolved| Ok(fs::metadata(resolved)?))
        .is_ok_and(|metadata| metadata.mode() & 0o111 != 0)
}

/// The recovery directories the preparation worker writes: the record
/// snapshot's (with its locks and binding), each receipt's, and the backup
/// root the backup command fills.
fn recovery_directories(maintenance: &Value) -> Vec<PathBuf> {
    let recovery = &maintenance["recovery"];
    ["snapshot_file", "receipt_file", "off_host_receipt_file"]
        .iter()
        .filter_map(|key| recovery[key].as_str())
        .filter_map(|file| Path::new(file).parent().map(Path::to_owned))
        .chain(recovery["backup_root"].as_str().map(PathBuf::from))
        .collect()
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

/// Read a file another writer controls through one descriptor: opened without
/// following a final symlink or blocking (a FIFO or device is refused, never
/// waited on), checked as a regular file and read up to `limit` bytes. The
/// descriptor's own metadata is returned for the caller's ownership checks.
fn read_pinned(path: &Path, limit: u64) -> Result<(fs::Metadata, Vec<u8>)> {
    let file = durable::open_regular(path, false)?;
    let metadata = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(format!(
            "{} exceeds its size limit",
            path.display()
        )));
    }
    Ok((metadata, bytes))
}

/// A transported receipt: a private mode-0600 regular file, read pinned.
pub(crate) fn receipt_bytes(path: &Path) -> Result<Vec<u8>> {
    let not_private = || invalid("transported receipt must be a private mode-0600 regular file");
    let (metadata, bytes) = read_pinned(path, super::recovery::MAX_OFF_HOST_RECEIPT_BYTES)
        .map_err(|_| not_private())?;
    if metadata.mode() & 0o777 != 0o600 {
        return Err(not_private());
    }
    Ok(bytes)
}

/// A transported receipt checked against its independently retained digest:
/// read once through a pinned descriptor (the sender can still swap the path).
fn verified_receipt(path: &Path, expected: &str) -> Result<(String, Vec<u8>)> {
    let expected = sha_hex(expected)?;
    let bytes = receipt_bytes(path)?;
    if codec::digest(&bytes) != expected || !codec::decode(&bytes)?.is_object() {
        return Err(invalid(
            "transported receipt differs from the independently retained SHA-256",
        ));
    }
    Ok((expected, bytes))
}

/// A directory opened without following a final symlink.
fn open_directory(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?)
}

const DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

/// Open `name` within the pinned directory `parent` (`openat`), never
/// following a symlink there.
fn open_at(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> std::io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = c_name(name)?;
    // SAFETY: a live descriptor and a NUL-terminated name; a returned
    // descriptor is owned exactly once by the File below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

fn c_name(name: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

/// The directory just made as `name` in `parent`, opened no-follow and proved
/// fresh before root adjusts it: root's and empty (read through its own
/// descriptor). A writer of `parent` could rename another one onto the name
/// in between; a non-empty or foreign one is refused.
fn fresh_at(parent: &fs::File, name: &std::ffi::OsStr) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    let directory = open_at(parent, name, DIRECTORY)?;
    let empty = (fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?)
        .next()
        .is_none();
    if directory.metadata()?.uid() != euid() || !empty {
        return Err(invalid(format!(
            "{} is not the directory just created",
            name.to_string_lossy()
        )));
    }
    Ok(directory)
}

/// `mkdirat` within the pinned directory `parent`.
fn make_at(parent: &fs::File, name: &std::ffi::OsStr, mode: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = c_name(name)?;
    // SAFETY: a live descriptor and a NUL-terminated name.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Rename `from` to `to` within the pinned directory `parent`, never
/// replacing an existing `to` (`renameat2(RENAME_NOREPLACE)`).
fn rename_at(
    parent: &fs::File,
    from: &std::ffi::OsStr,
    to: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (from, to) = (c_name(from)?, c_name(to)?);
    let fd = parent.as_raw_fd();
    // SAFETY: a live descriptor and NUL-terminated names.
    if unsafe { libc::renameat2(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_NOREPLACE) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
        let valid_unit = |unit: &String| valid_unit(unit);
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
            || !manifest.execution_lock.as_deref().is_none_or(dedicated)
        {
            return Err(invalid("invalid rollout manifest or primary binding"));
        }
        // Source-local repositories prepare from a selected capture without a
        // preparation binding the coordinator could hand off; not supported.
        if !matches!(
            super::recovery_repository::source_local(&maintenance["recovery"]),
            Ok(false)
        ) {
            return Err(invalid(
                "the rollout coordinator requires the legacy recovery repository protocol",
            ));
        }
        // Only clients are enumerated from active units, so only they may be
        // templates; publishers, receivers and setup units are acted on by name.
        if (manifest.paused_publishers.iter())
            .chain(&manifest.receiver_units)
            .chain(&manifest.setup_units)
            .any(|unit| unit.ends_with("@.service"))
        {
            return Err(invalid(
                "paused publishers, receivers and setup units must name units, not templates",
            ));
        }
        // One owner and mode contract per provisioned path (an export root
        // also listed as a generic directory could satisfy neither).
        let provisions = &manifest.prepare.provision_directories;
        for (index, a) in provisions.iter().enumerate() {
            if provisions[index + 1..]
                .iter()
                .any(|b| b.path == a.path && (b.group != a.group || b.mode != a.mode))
            {
                return Err(invalid(format!(
                    "{} is provisioned twice with different owners or modes",
                    a.path.display()
                )));
            }
        }
        // Each unit has one role: a retired client that is also a paused
        // publisher would be restarted when the pause lifts, a receiver that is
        // also a client restarted by both. Templates cover their instances.
        let primary = [maintenance["startup_inhibition"]["unit"]
            .as_str()
            .unwrap_or("postgresql.service")
            .to_owned()];
        let roles: [Vec<&String>; 5] = [
            manifest
                .clients
                .iter()
                .chain(&manifest.retired_clients)
                .collect(),
            manifest.paused_publishers.iter().collect(),
            manifest.receiver_units.iter().collect(),
            manifest.setup_units.iter().collect(),
            primary.iter().collect(),
        ];
        let overlap = |a: &str, b: &str| a == b || unit_matches(a, b) || unit_matches(b, a);
        for (index, role) in roles.iter().enumerate() {
            for other in &roles[index + 1..] {
                if let Some((a, b)) = (role.iter())
                    .flat_map(|a| other.iter().map(move |b| (a, b)))
                    .find(|(a, b)| overlap(a, b))
                {
                    return Err(invalid(format!(
                        "rollout units must each have one role; {a} and {b} overlap"
                    )));
                }
            }
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
        let export = prepare.export_root.as_path();
        if !dedicated(export) {
            return Err(invalid("rollout export root must be a dedicated directory"));
        }
        // The roots post-fence steps write are pairwise disjoint (the writer
        // fence lives in the identity state directory), except the intended
        // nestings: a provisioned directory inside a writable path or another
        // provisioned one (evidence in its backup root), the export root
        // inside a writable path. A shared root would be re-owned, hidden or
        // made untraversable by another step after the fence.
        let slot = (manifest.import_slot.parent())
            .ok_or_else(|| invalid("import slot has no directory"))?;
        let roots: Vec<(&str, &Path)> = [
            (
                "PostgreSQL data directory",
                super::string(&maintenance, "data_dir")?,
            ),
            (
                "identity state directory",
                super::string(&maintenance, "state_dir")?,
            ),
            (
                "startup inhibition state directory",
                super::string(&maintenance["startup_inhibition"], "state_dir")?,
            ),
            ("rollout journal root", STATE_ROOT),
        ]
        .map(|(label, path)| (label, Path::new(path)))
        .into_iter()
        .chain([("import slot directory", slot), ("export root", export)])
        .chain((prepare.read_write_paths.iter()).map(|path| ("writable path", path.as_path())))
        .chain(
            (prepare.provision_directories.iter())
                .filter(|directory| directory.path != export)
                .map(|directory| ("provisioned directory", directory.path.as_path())),
        )
        .collect();
        let intended = |(a, inner): (&str, &Path), (b, outer): (&str, &Path)| {
            inner.starts_with(outer)
                && matches!(
                    (a, b),
                    (
                        "provisioned directory",
                        "writable path" | "provisioned directory"
                    ) | ("export root", "writable path")
                )
        };
        for (index, &a) in roots.iter().enumerate() {
            if let Some(&b) = roots[index + 1..]
                .iter()
                .find(|&&b| overlaps(a.1, b.1) && !intended(a, b) && !intended(b, a))
            {
                return Err(invalid(format!(
                    "rollout {} {} and {} {} overlap; give each its own tree",
                    a.0,
                    a.1.display(),
                    b.0,
                    b.1.display()
                )));
            }
        }
        // The worker writes the evidence and provisioned directories: outside
        // its writable paths they are read-only to it (the export root it is
        // always granted).
        let writable =
            |path: &Path| (prepare.read_write_paths.iter()).any(|root| path.starts_with(root));
        if let Some(path) = (prepare.provision_directories.iter())
            .map(|directory| directory.path.clone())
            .filter(|path| path != export)
            .chain(recovery_directories(&maintenance))
            .find(|path| !writable(path))
        {
            return Err(invalid(format!(
                "{} is written by the preparation worker but lies outside its writable paths",
                path.display()
            )));
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
        // What post-fence steps run or read is checked now, not when first
        // used: each local executable (thaw's cutover checker included) is an
        // executable regular store file, the worker's preparation config a
        // store file. The executor command runs on another host: lexical only.
        if !([&manifest.harbor, &manifest.systemctl, &manifest.systemd_run].into_iter())
            .chain(manifest.cutover.as_ref().map(|cutover| &cutover.checker))
            .chain(manifest.executor.transport.as_ref())
            .all(|path| store_executable(path))
            || durable::immutable_store_reference(&manifest.preparation).is_err()
        {
            return Err(invalid(
                "rollout harbor, systemctl, systemd-run, cutover checker and transport hook must be executable regular store files and the preparation config a store file",
            ));
        }
        let executor = &manifest.executor;
        // Certification is independent only on another host, and `certify`
        // refuses root there: both are refused before the fence.
        if executor.host.is_empty()
            || executor.host == manifest.host
            || executor.user.is_empty()
            || executor.user == "root"
            || !executor
                .command
                .first()
                .is_some_and(|program| program.starts_with('/'))
            || executor
                .command
                .iter()
                .any(|arg| arg.is_empty() || arg.contains('\0'))
        {
            return Err(invalid(
                "rollout executor needs a host other than the primary, a non-root user and an absolute command of non-empty arguments",
            ));
        }
        Ok(Self {
            group: group_id,
            gcroots: PathBuf::from(GC_ROOTS),
            digest: codec::file_digest(config)?,
            maintenance,
            manifest,
            config: config.to_owned(),
            state: PathBuf::from(STATE_ROOT),
            installed: PathBuf::from(INSTALLED),
            anchor: PathBuf::from("/"),
            service_owner: None,
            profile: PathBuf::from(PROFILE),
            booted: PathBuf::from(BOOTED),
        })
    }

    fn journal_path(&self) -> PathBuf {
        self.state.join("rollout.json")
    }

    fn journal(&self) -> Result<Option<Journal>> {
        // An unreadable journal is an error, never an idle rollout.
        let journal = load_journal(&self.journal_path())?;
        if journal
            .as_ref()
            .is_some_and(|journal| journal.system_identifier != self.manifest.system_identifier)
        {
            return Err(invalid(
                "retained rollout journal belongs to another primary",
            ));
        }
        Ok(journal)
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
            pending_clients: Vec::new(),
            unloaded: Vec::new(),
            inhibiting: false,
            fence_opening: false,
            reopened: false,
            opening_config: Some(self.config.clone()),
            transported: None,
        }
    }

    /// Only the manifest that opened a journal steps or unwinds it: every
    /// policy its fence, inhibition, clients and slots were made under is that
    /// manifest's. It is rooted against GC while the journal is open, but never
    /// loaded implicitly; name it instead.
    fn require_opener(&self, journal: &Journal) -> Result<()> {
        if journal.manifest_sha256 == self.digest {
            return Ok(());
        }
        let opener = journal.opening_config.as_deref().map_or_else(
            || "<the opening manifest>".into(),
            |path| path.display().to_string(),
        );
        Err(invalid(format!(
            "this rollout was opened by {opener}; rerun with `--config {opener}` (canix: \
             `--candidate <generation>`)"
        )))
    }

    fn bound(&self) -> Result<Journal> {
        let journal = self.journal()?.unwrap_or_else(|| self.fresh_journal());
        self.require_opener(&journal)?;
        Ok(journal)
    }

    /// Root the opening manifest (and so its closure) against GC while its
    /// journal exists: swapped in atomically, removed when it is archived.
    fn root_opening_manifest(&self) -> Result<()> {
        let staging = self
            .gcroots
            .join(format!(".{GC_ROOT}.{}", std::process::id()));
        let _ = fs::remove_file(&staging);
        std::os::unix::fs::symlink(&self.config, &staging)?;
        fs::rename(&staging, self.gcroots.join(GC_ROOT))?;
        // Durable before the journal that relies on it is first written.
        durable::sync_directory(&self.gcroots)
    }

    /// Fail unless every declared mount (the worker's or the maintenance
    /// manifest's) covering `path` is mounted.
    fn require_covering_mounts(&self, path: &Path) -> Result<()> {
        let maintenance = (self.maintenance["required_mounts"].as_array().into_iter())
            .flatten()
            .filter_map(Value::as_str)
            .map(Path::new);
        let covering: Vec<_> = (self.manifest.prepare.required_mounts.iter())
            .map(PathBuf::as_path)
            .chain(maintenance)
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
    /// before systemd or a root write resolves the path. The walk itself never
    /// re-resolves a pathname: each component is opened (`openat`, no-follow)
    /// from its parent's descriptor and checked through its own (`fstat`).
    /// With `create` (the leaf's mode), a missing component is made by one
    /// `mkdirat` (intermediates 0755 so service users can traverse them),
    /// adjusted through a no-follow descriptor once it proves to be ours, and
    /// never made below a directory others can write. Returns the leaf's
    /// pinned descriptor and metadata when it exists.
    fn walk(
        &self,
        path: &Path,
        holders: &[&Path],
        create: Option<u32>,
    ) -> Result<Option<(fs::File, fs::Metadata)>> {
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
        let mut directory = open_directory(&current)?;
        let mut parent_shared = false;
        for component in std::iter::once(None).chain(below.components().map(Some)) {
            current.extend(component);
            let leaf = current == *path;
            if let Some(component) = component {
                let name = component.as_os_str();
                directory = match open_at(&directory, name, DIRECTORY) {
                    Ok(child) => child,
                    // Absent, and so creatable only below a directory others
                    // cannot write (where its name could be raced).
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let Some(mode) = create.filter(|_| !parent_shared) else {
                            return if parent_shared {
                                Err(untrusted(&current))
                            } else {
                                Ok(None)
                            };
                        };
                        let mode = if leaf { mode } else { 0o755 };
                        make_at(&directory, name, mode)?;
                        let child = fresh_at(&directory, name).map_err(|_| untrusted(&current))?;
                        child.set_permissions(fs::Permissions::from_mode(mode))?;
                        child
                    }
                    // A symlink (ELOOP) or a non-directory (ENOTDIR).
                    Err(error)
                        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) =>
                    {
                        return Err(untrusted(&current));
                    }
                    Err(error) => return Err(error.into()),
                };
            }
            let metadata = directory.metadata()?;
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
        }
        let metadata = directory.metadata()?;
        Ok(Some((directory, metadata)))
    }

    /// Create the journal root privately, but never re-permission an existing
    /// one: it must already be a private directory this process owns.
    fn state_dir(&self) -> Result<()> {
        // Never create the journal on the root filesystem below an absent mount.
        self.require_covering_mounts(&self.state)?;
        let (_, leaf) = self
            .walk(&self.state, &[], Some(0o700))?
            .ok_or_else(|| invalid("rollout journal root is missing"))?;
        if leaf.uid() != euid() || leaf.mode() & 0o077 != 0 {
            return Err(invalid(
                "rollout journal root must be a private directory owned by the coordinator",
            ));
        }
        Ok(())
    }

    /// Directories that may hold further worker paths whatever their owner:
    /// the declared provisioned ones and writable roots (postgres may own
    /// them; what root writes or reads below them is pinned no-follow).
    fn writable_roots(&self) -> Vec<&Path> {
        let prepare = &self.manifest.prepare;
        (prepare.provision_directories.iter())
            .map(|directory| directory.path.as_path())
            .chain(prepare.read_write_paths.iter().map(PathBuf::as_path))
            .collect()
    }

    /// The export root inside a writable root is writable through it; only
    /// one outside every writable root is its own sandbox entry.
    fn export_sandboxed(&self) -> bool {
        let prepare = &self.manifest.prepare;
        !(prepare.read_write_paths.iter()).any(|root| prepare.export_root.starts_with(root))
    }

    /// systemd resolves symlinks in the worker's sandbox paths, granting the
    /// target write access: every one must lie on a chain no one else can swap.
    fn check_sandbox_paths(&self) -> Result<()> {
        let prepare = &self.manifest.prepare;
        for path in (prepare.read_write_paths.iter())
            .chain(self.export_sandboxed().then_some(&prepare.export_root))
            .chain(&prepare.required_mounts)
        {
            self.walk(path, &[], None)?;
        }
        Ok(())
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        self.state_dir()?;
        if !writer_fence::exists(&self.journal_path()) {
            self.root_opening_manifest()?;
        }
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

    /// Whether `unit` has a queued job. systemctl renders none as an empty
    /// value (systemd 261); `0`, the D-Bus no-job id, counts as none too.
    fn has_job(&self, unit: &str) -> Result<bool> {
        Ok(!matches!(self.show(unit, "Job")?.as_str(), "" | "0"))
    }

    /// Whether the deterministic preparation worker is still loaded and running.
    /// A never-loaded or collected (`--collect`) worker reads `not-found`
    /// (systemctl still exits 0) and is idle; any other unknown state is busy.
    fn preparing(&self) -> Result<bool> {
        let (loaded, state) = self.unit_state(PREPARE_UNIT)?;
        Ok(loaded && restorable(&state))
    }

    /// Likewise for the executor transport hook's unit.
    fn transporting(&self) -> Result<bool> {
        let (loaded, state) = self.unit_state(TRANSPORT_UNIT)?;
        Ok(loaded && restorable(&state))
    }

    /// Whether `unit` is loaded (not `not-found`, which systemctl reports with
    /// exit 0 for a never-loaded or collected unit) and its ActiveState.
    fn unit_state(&self, unit: &str) -> Result<(bool, String)> {
        let output = self.systemctl(&["show", unit, "--property=LoadState,ActiveState"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap_or_default()
                .to_owned()
        };
        Ok((value("LoadState=") != "not-found", value("ActiveState=")))
    }

    fn active_clients(&self) -> Result<Vec<String>> {
        let patterns = || {
            self.manifest
                .clients
                .iter()
                .chain(&self.manifest.retired_clients)
        };
        let mut clients: Vec<String> = (self.services(false)?.into_iter())
            .filter(|(unit, state)| {
                restorable(state) && patterns().any(|pattern| unit_matches(pattern, unit))
            })
            .map(|(unit, _)| unit)
            .collect();
        clients.sort();
        clients.dedup();
        Ok(clients)
    }

    /// Listed services and their ActiveState (UNIT LOAD ACTIVE ...): those
    /// active, with a job or failed, or with `all` every loaded one.
    fn services(&self, all: bool) -> Result<Vec<(String, String)>> {
        let mut args = vec!["list-units", "--type=service", "--plain", "--no-legend"];
        args.extend(["--no-pager"].into_iter().chain(all.then_some("--all")));
        let output = self.systemctl(&args)?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                let unit = columns.next()?.to_owned();
                Some((unit, columns.nth(1)?.to_owned()))
            })
            .collect())
    }

    /// A deliberate start: clear the start limit earlier rollout cycles may have
    /// spent, or a retried unwind leaves the stopped unit down.
    fn start(&self, unit: &str) -> Result<()> {
        let _ = self.systemctl(&["reset-failed", "--", unit]);
        self.systemctl(&["start", "--", unit]).map(drop)
    }

    /// Start a client, receiver or publisher without waiting for it to finish:
    /// a oneshot (a base backup can run for an hour) completes only when its
    /// command exits, which would outlast any bound. Only a failed start is an
    /// error: a unit running, ended, or still queued at the bound (behind
    /// another unit's job) is started. A unit the running generation does not
    /// load (one the candidate removed, or only it brings) is nothing to
    /// restore, as for the pause and the primary's stop: `Ok(false)`.
    fn start_queued(&self, unit: &str) -> Result<bool> {
        if !self.unit_state(unit)?.0 {
            return Ok(false);
        }
        let _ = self.systemctl(&["reset-failed", "--", unit]);
        self.systemctl(&["--no-block", "start", "--", unit])
            .map_err(|error| invalid(format!("starting {unit}: {error}")))?;
        let deadline = Instant::now() + Duration::from_secs(self.manifest.setup_settle_seconds);
        loop {
            let (_, state) = self.unit_state(unit)?;
            match state.as_str() {
                "active" | "activating" | "reloading" => return Ok(true),
                "failed" => return Err(invalid(format!("{unit} failed to start"))),
                // With no job left a oneshot ended; one still queued at the
                // bound waits on another job.
                _ if !self.has_job(unit)? || Instant::now() >= deadline => return Ok(true),
                _ => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    fn stop(&self, unit: &str) -> Result<()> {
        self.systemctl(&["stop", "--", unit])?;
        match self.show(unit, "ActiveState")? {
            state if restorable(&state) => Err(invalid(format!("{unit} did not stop ({state})"))),
            _ => Ok(()),
        }
    }

    /// Start the primary, then wait for the declared setup units: they connect
    /// as the control role right after startup and must not race inspection.
    /// Stopping the primary also stops every unit that Requires or BindsTo it
    /// (through targets such as `postgresql.target`). Record the active ones
    /// first so reopen restores them: as clients, or as pending receivers.
    /// Retired clients, setup units and paused publishers are not restored
    /// this way (they have their own handling); only services are recorded.
    fn stop_primary(&self, journal: &mut Journal) -> Result<()> {
        let primary = self.primary_unit().to_owned();
        let mut seen = vec![primary.clone()];
        let mut queue = vec![primary.clone()];
        let mut recorded = false;
        while let Some(unit) = queue.pop() {
            let output =
                self.systemctl(&["show", &unit, "--property=RequiredBy,BoundBy,ConsistsOf"])?;
            let dependents: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_once('=').map(|(_, units)| units.to_owned()))
                .flat_map(|units| {
                    units
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .collect();
            for dependent in dependents {
                if seen.contains(&dependent) {
                    continue;
                }
                seen.push(dependent.clone());
                queue.push(dependent.clone());
                let excluded = !valid_unit(&dependent)
                    || (self.manifest.retired_clients.iter())
                        .any(|pattern| unit_matches(pattern, &dependent))
                    || self.manifest.setup_units.contains(&dependent)
                    || self.manifest.paused_publishers.contains(&dependent);
                if excluded {
                    continue;
                }
                let (loaded, state) = self.unit_state(&dependent)?;
                if !loaded || !restorable(&state) {
                    continue;
                }
                let list = if self.manifest.receiver_units.contains(&dependent) {
                    &mut journal.pending_receivers
                } else {
                    &mut journal.clients
                };
                if !list.contains(&dependent) {
                    list.push(dependent);
                    recorded = true;
                }
            }
        }
        if recorded {
            journal.clients.sort();
            self.save(journal)?;
        }
        self.stop(&primary)
    }

    /// Fencing requires setup to have succeeded (a oneshot ends `inactive`);
    /// unwinding only waits, since refusing there would keep writers down.
    fn start_primary(&self, require_setup: bool) -> Result<()> {
        self.start(self.primary_unit())?;
        let deadline = Instant::now() + Duration::from_secs(self.manifest.setup_settle_seconds);
        loop {
            let mut busy = false;
            for unit in &self.manifest.setup_units {
                let state = self.show(unit, "ActiveState")?;
                if require_setup && state == "failed" {
                    return Err(invalid(format!(
                        "setup unit {unit} failed; the writer window is retained: inspect it \
                         (journalctl -u {unit}), fix and restart it, then rerun"
                    )));
                }
                busy |= self.has_job(unit)?
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
        // A crash after Harbor inhibits but before the token is saved must
        // still leave a journal `abort` can recover the token through.
        if !journal.inhibiting {
            journal.inhibiting = true;
            self.save(journal)?;
        }
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
        let record = codec::decode(&read_pinned(&path, MAX_RECORD_BYTES)?.1)?;
        Ok(Some(super::string(&record, "token")?.to_owned()))
    }

    /// The token of any writer fence Harbor holds for this primary, whoever
    /// opened it: the active record, or an interrupted acquisition's selector.
    fn live_fence(&self) -> Result<Option<String>> {
        // Both live in postgres-writable storage: read pinned and bounded.
        let marker = writer_fence::marker(&self.maintenance)?;
        if writer_fence::exists(&marker) {
            let record = codec::decode(&read_pinned(&marker, MAX_RECORD_BYTES)?.1)?;
            return Ok(Some(super::string(&record, "token")?.to_owned()));
        }
        let auto =
            Path::new(super::string(&self.maintenance, "data_dir")?).join("postgresql.auto.conf");
        if !writer_fence::exists(&auto) {
            return Ok(None);
        }
        let bytes = read_pinned(&auto, MAX_RECORD_BYTES)?.1;
        Ok(bytes
            .split(|b| *b == b'\n')
            .find_map(|line| line.strip_prefix(b"# harbor-db-writer-fence="))
            .map(|token| String::from_utf8_lossy(token).into_owned()))
    }

    /// Refuse a live fence this rollout did not open (another coordinator's),
    /// unless it recorded opening one whose token it has yet to learn.
    fn refuse_foreign_fence(&self, journal: &Journal, adopt: Option<&str>) -> Result<()> {
        let Some(token) = self.live_fence()? else {
            return Ok(());
        };
        if journal.fence_token.as_deref() == Some(token.as_str()) {
            return Ok(());
        }
        if !journal.fence_opening {
            return Err(invalid(format!(
                "Harbor holds writer fence {token}, which this rollout did not open; \
                 finish it with the coordinator that opened it first"
            )));
        }
        // Interrupted between recording the intent and the token: Harbor mints
        // the token, so nothing proves this fence is ours rather than one
        // another coordinator opened meanwhile. Only the operator can say.
        if adopt != Some(token.as_str()) {
            return Err(invalid(format!(
                "this rollout was interrupted while opening a writer fence and Harbor now holds fence \
                 {token}, which it cannot prove it opened; only after confirming no other coordinator \
                 (such as a legacy writer-fence-acquire and its transition journal) ran since, rerun \
                 with `--adopt-fence-token {token}`"
            )));
        }
        Ok(())
    }

    /// Open (or resume) the writer fence. Harbor resumes any prepared fence, so
    /// first refuse one this rollout did not open (another coordinator's) or,
    /// after an interrupted opening, one the operator did not adopt.
    fn open_fence(&self, journal: &mut Journal, adopt: Option<&str>) -> Result<()> {
        self.refuse_foreign_fence(journal, adopt)?;
        if !journal.fence_opening {
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
        let token = super::string(&value, "token")?;
        if adopt.is_some_and(|adopted| adopted != token) {
            return Err(invalid(
                "Harbor opened a different fence than the adopted one",
            ));
        }
        journal.fence_token = Some(token.to_owned());
        self.save(journal)
    }

    /// `--adopt-fence-token` only resolves an opening interrupted before its
    /// token was recorded (a lost journal is adopted by `abort` separately).
    fn adoptable(journal: &Journal, adopt: Option<&str>) -> Result<()> {
        if adopt.is_some() && !(journal.fence_opening && journal.fence_token.is_none()) {
            return Err(invalid(
                "--adopt-fence-token applies only to a fence opening interrupted before its token \
                 was recorded",
            ));
        }
        Ok(())
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
        // Written once: its mtime is when the pause began.
        if !self.pause_marker().exists() {
            durable::atomic_write_mode(&self.pause_marker(), b"", 0o600)?;
        }
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
            // A publisher the running generation lacks (the candidate brings
            // it) is not loaded: nothing to stop, and `stop` would exit 5.
            let (loaded, state) = self.unit_state(unit)?;
            if !loaded || !restorable(&state) {
                continue;
            }
            // Recorded before stopping, so abort or thaw restarts it.
            if !journal.stopped_publishers.contains(unit) {
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
        // A declared publisher whose start the pause skipped (one the candidate
        // brought, or a timer that fired meanwhile) is restarted too; never
        // one whose last failed condition predates the pause. Recorded before
        // the pause lifts, so a retry still restarts it.
        if let Ok(marker) = fs::symlink_metadata(self.pause_marker()) {
            for unit in &self.manifest.paused_publishers {
                if !journal.stopped_publishers.contains(unit)
                    && self.skipped_since(unit, marker.mtime())?
                {
                    journal.stopped_publishers.push(unit.clone());
                    self.save(journal)?;
                }
            }
        }
        let mut lifted = false;
        if !self.manifest.paused_publishers.is_empty() {
            let root = self.drop_in_root()?;
            for unit in &self.manifest.paused_publishers {
                let file = root.join(format!("{unit}.d")).join(PAUSE_DROP_IN);
                if fs::symlink_metadata(&file).is_ok() {
                    fs::remove_file(&file)?;
                    lifted = true;
                }
            }
        }
        if self.pause_marker().exists() {
            fs::remove_file(self.pause_marker())?;
            lifted = true;
        }
        if lifted {
            self.systemctl(&["daemon-reload"])?;
        }
        let restarted = self.restart(journal, |journal| &mut journal.stopped_publishers);
        self.save(journal)?;
        restarted
    }

    /// Whether `unit` is loaded and its last start was skipped by a failed
    /// condition at or after `since` (Unix seconds).
    fn skipped_since(&self, unit: &str, since: i64) -> Result<bool> {
        let output = self.systemctl(&[
            "show",
            unit,
            "--timestamp=unix",
            "--property=LoadState,ConditionResult,ConditionTimestamp",
        ])?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap_or_default()
        };
        Ok(value("LoadState=") == "loaded"
            && value("ConditionResult=") == "no"
            && (value("ConditionTimestamp=@").parse::<i64>()).is_ok_and(|at| at >= since))
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

    /// Provision the declared directories shallowest first (stable), so a
    /// nested one never creates its declared parent as a root intermediate.
    fn provision_all(&self) -> Result<()> {
        let mut directories: Vec<&Provision> =
            self.manifest.prepare.provision_directories.iter().collect();
        directories.sort_by_key(|directory| directory.path.components().count());
        for directory in directories {
            self.provision(&directory.path, &directory.group, directory.mode)?;
        }
        Ok(())
    }

    /// The owner a provisioned directory of `group` must have.
    fn provision_owner(&self, group: &str) -> Result<(u32, u32)> {
        // Resolved even where tests stand in for the owner: an unknown group
        // must fail the preflight, never the provisioning after the fence.
        let gid = (self.group)(group)?;
        Ok(match self.service_owner {
            Some(owner) => owner,
            None => (account("postgres")?.0, gid),
        })
    }

    /// The uid and groups postgres runs a step with: Harbor's verbs with its
    /// account's groups, the worker also with the declared supplementary ones.
    fn service_identity(&self, worker: bool) -> Result<(u32, Vec<u32>)> {
        let (uid, mut groups) = match self.service_owner {
            Some((uid, gid)) => (uid, vec![gid]),
            None => {
                let identity = process::account("postgres")?;
                (identity.uid, identity.groups)
            }
        };
        if worker {
            for group in &self.manifest.prepare.supplementary_groups {
                groups.push((self.group)(group)?);
            }
        }
        Ok((uid, groups))
    }

    /// Every existing directory on `path` (below the anchor, `/` on hosts)
    /// must let `identity` search it, or the step using it fails after the fence.
    fn require_traversable(&self, path: &Path, (uid, groups): &(u32, Vec<u32>)) -> Result<()> {
        for directory in path.ancestors().take_while(|at| *at != self.anchor) {
            let Ok(metadata) = fs::symlink_metadata(directory) else {
                continue;
            };
            let search = if metadata.uid() == *uid {
                0o100
            } else if groups.contains(&metadata.gid()) {
                0o010
            } else {
                0o001
            };
            if metadata.is_dir() && metadata.mode() & search == 0 {
                return Err(invalid(format!(
                    "{} is not searchable by postgres at {}; grant it search permission first",
                    path.display(),
                    directory.display()
                )));
            }
        }
        Ok(())
    }

    /// Everything static that could fail a step once the fence is held. Every
    /// declared mount is present (the worker requires them all). Each
    /// provisioned directory, and the identity state directory Harbor's fence
    /// and adoption write as postgres, either already has its owner, group and
    /// mode (an existing one, perhaps a mountpoint, is never re-owned) or is
    /// creatable under trusted ancestry. Every writable path, evidence
    /// directory and the export root exists or is provisioned (systemd fails
    /// a missing writable path; Harbor creates no evidence directory). All are
    /// searchable by the postgres identity using them.
    fn preflight_paths(&self) -> Result<()> {
        let prepare = &self.manifest.prepare;
        super::pg_core::require_mounts(&json!({ "required_mounts": prepare.required_mounts }))?;
        // The maintenance manifest's own, which every Harbor verb requires:
        // `fence-open` (and the abort that must reopen) first.
        super::pg_core::require_mounts(&self.maintenance)?;
        let holders: Vec<&Path> = (prepare.provision_directories.iter())
            .map(|directory| directory.path.as_path())
            .collect();
        // Walked below declared directories and writable roots, whatever
        // their owner (postgres may own them).
        let roots = self.writable_roots();
        let worker = self.service_identity(true)?;
        let harbor = self.service_identity(false)?;
        let authority = Path::new(super::string(&self.maintenance, "state_dir")?);
        let provisioned = (prepare.provision_directories.iter())
            .map(|directory| {
                let path = directory.path.as_path();
                (path, directory.group.as_str(), directory.mode, &worker)
            })
            .chain([(authority, "postgres", 0o700, &harbor)]);
        for (path, group, mode, identity) in provisioned {
            let (uid, gid) = self.provision_owner(group)?;
            if let Some((_, metadata)) = self.walk(path, &roots, None)?
                && (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777) != (uid, gid, mode)
            {
                return Err(invalid(format!(
                    "{} exists but is not {uid}:{gid} mode {mode:o}; fix it first (an \
                     existing directory is never re-owned)",
                    path.display()
                )));
            }
            self.require_traversable(path, identity)?;
        }
        // Every directory the worker writes is reached without a redirected
        // component (walked no-follow below its writable root, which postgres
        // may own) and is writable by the worker.
        for path in (prepare.read_write_paths.iter().cloned())
            .chain(recovery_directories(&self.maintenance))
            .chain([prepare.export_root.clone()])
        {
            self.require_traversable(&path, &worker)?;
            match self.walk(&path, &roots, None)? {
                Some((_, metadata)) => require_writable(&metadata, &path, &worker)?,
                // Provisioned with the worker as owner before it starts.
                None if holders.contains(&path.as_path()) => {}
                None => {
                    return Err(invalid(format!(
                        "{} neither exists nor is provisioned, so the preparation worker would fail after the fence; create it first",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// A directory a post-fence step writes as root: under trusted ancestry,
    /// an existing one owned by root and writable by no one else (`mask`, as
    /// the writing step checks), a missing one creatable.
    fn check_root_directory(&self, path: &Path, mask: u32, owned: &str) -> Result<()> {
        if let Some((_, metadata)) = self.walk(path, &[], None)?
            && (metadata.uid() != euid() || metadata.mode() & mask != 0)
        {
            return Err(invalid(format!("{} must be {owned}", path.display())));
        }
        Ok(())
    }

    /// The record snapshot and preparation binding Harbor's preparation left
    /// (postgres-written: read pinned and bounded).
    fn evidence(&self) -> Result<[(PathBuf, Option<Value>); 2]> {
        let read = |path: PathBuf| -> Result<(PathBuf, Option<Value>)> {
            if !writer_fence::exists(&path) {
                return Ok((path, None));
            }
            let value = codec::decode(&read_pinned(&path, MAX_EVIDENCE_BYTES)?.1)?;
            Ok((path, Some(value)))
        };
        let snapshot = super::string(&self.maintenance["recovery"], "snapshot_file")?;
        Ok([
            read(PathBuf::from(snapshot))?,
            read(self.evidence_dir()?.join("preparation.json"))?,
        ])
    }

    /// Before the fence no evidence can be this window's yet: a snapshot or
    /// preparation binding from another window (or none) would only fail
    /// `prepare` once production is fenced. It is never removed implicitly.
    fn refuse_stale_evidence(&self, journal: &Journal) -> Result<()> {
        let [snapshot, preparation] = self.evidence()?;
        let resumed = snapshot.1.as_ref().is_some_and(|value| {
            journal.fence_token.is_some()
                && value["writer_fence_token"].as_str() == journal.fence_token.as_deref()
        });
        // A receipt without its bound snapshot fails preparation too.
        let receipts = ["receipt_file", "off_host_receipt_file"]
            .iter()
            .filter_map(|key| self.maintenance["recovery"][key].as_str())
            .map(PathBuf::from)
            .filter(|path| writer_fence::exists(path));
        let stale: Vec<String> = [snapshot, preparation]
            .into_iter()
            .filter(|(_, value)| value.is_some())
            .map(|(path, _)| path)
            .chain(receipts)
            .filter(|_| !resumed)
            .map(|path| path.display().to_string())
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        Err(invalid(format!(
            "recovery evidence from another writer window ({}) would fail preparation after the \
             fence; move it aside, then rerun",
            stale.join(", ")
        )))
    }

    /// The prepared evidence is this rollout's only if its snapshot is bound to
    /// this rollout's fence and its preparation binding is that snapshot's.
    fn require_bound_evidence(&self, journal: &Journal) -> Result<()> {
        let [(_, snapshot), (_, preparation)] = self.evidence()?;
        let (Some(snapshot), Some(preparation)) = (snapshot, preparation) else {
            return Err(invalid("preparation left no bound record snapshot"));
        };
        let bound = journal.fence_token.is_some()
            && snapshot["writer_fence_token"].as_str() == journal.fence_token.as_deref()
            && (preparation.as_object()).is_some_and(|fields| {
                fields
                    .iter()
                    .all(|(key, value)| snapshot.get(key) == Some(value))
            });
        if !bound {
            return Err(invalid(
                "the prepared evidence is bound to another writer window (stale); abort, move it \
                 aside and fence again",
            ));
        }
        Ok(())
    }

    /// Staging the receipt needs a root-private directory under root-owned
    /// ancestry (an existing one must already be, an absent one creatable)
    /// and an empty slot: a receipt this rollout never recorded fails `prepare`.
    fn check_slot_directory(&self) -> Result<()> {
        let slot = &self.manifest.import_slot;
        let directory = slot
            .parent()
            .ok_or_else(|| invalid("import slot has no directory"))?;
        self.check_root_directory(directory, 0o077, "private to root")?;
        if fs::symlink_metadata(slot).is_ok() {
            return Err(invalid(format!(
                "import slot {} holds a receipt this rollout did not stage; move it aside, then rerun",
                slot.display()
            )));
        }
        Ok(())
    }

    /// Create a service directory as root without ever following a symlink.
    fn provision(&self, path: &Path, group: &str, mode: u32) -> Result<()> {
        // Never provision on the root filesystem below an absent mount.
        self.require_covering_mounts(path)?;
        // A declared directory or writable root may hold further ones (an
        // evidence directory in its backup root): those are made by mkdir
        // inside it and adjusted through a no-follow descriptor, so its owner
        // cannot redirect them.
        let holders = self.writable_roots();
        let owner = self.provision_owner(group)?;
        let parent_path =
            (path.parent()).ok_or_else(|| invalid("provisioned directory has no parent"))?;
        // Pin the parent once; everything after is relative to its descriptor,
        // so a parent's owner (postgres, for evidence in its backup root)
        // renaming paths can never redirect a root mkdir, chown or rename.
        let (parent, metadata) = (self.walk(parent_path, &holders, Some(0o755))?)
            .ok_or_else(|| invalid("provisioned directory has no parent"))?;
        // Created only in a directory no one else can write, or a declared one.
        if !([0, euid()].contains(&metadata.uid()) || holders.contains(&parent_path))
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid(format!(
                "{} has untrusted ancestry at {}",
                path.display(),
                parent_path.display()
            )));
        }
        self.provision_in(&parent, path, owner, mode)
    }

    /// Provision `path` inside its pinned `parent`. Published only once
    /// complete: made, permissioned and owned under a random private name
    /// (no writer of `parent` can take it first, and no leftover is ever
    /// reused), then renamed into place without replacing anything, so a
    /// crash never leaves it root-owned where a retry would accept it as
    /// provisioned.
    // ponytail: an interrupted attempt leaves its empty staging directory
    // (ignored); remove such leftovers by hand.
    fn provision_in(
        &self,
        parent: &fs::File,
        path: &Path,
        (uid, gid): (u32, u32),
        mode: u32,
    ) -> Result<()> {
        let name =
            (path.file_name()).ok_or_else(|| invalid("provisioned directory has no name"))?;
        let untrusted = || invalid(format!("{} has untrusted ancestry", path.display()));
        // An existing one's owner and mode were verified by the preflight
        // through this same kind of no-follow descriptor.
        match open_at(parent, name, DIRECTORY) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                return Err(untrusted());
            }
            Err(error) => return Err(error.into()),
        }
        let staging = std::ffi::OsString::from(format!(
            ".{}.{}.provisioning",
            name.to_string_lossy(),
            writer_fence::token()?
        ));
        make_at(parent, &staging, 0o700)?;
        let directory = fresh_at(parent, &staging).map_err(|_| untrusted())?;
        directory.set_permissions(fs::Permissions::from_mode(mode))?;
        std::os::unix::fs::fchown(&directory, Some(uid), Some(gid))?;
        rename_at(parent, &staging, name)?;
        Ok(parent.sync_all()?)
    }

    fn fence(&self, journal: &mut Journal, adopt: Option<&str>) -> Result<()> {
        if journal.phase != Phase::Idle {
            // Resumed: re-stop any newly started declared client and re-verify.
            self.stop_clients(journal)?;
            return self.inspect_fence(journal);
        }
        // Before any side effect: a refusal must leave production untouched.
        self.refuse_foreign_fence(journal, adopt)?;
        self.state_dir()?;
        self.preflight_paths()?;
        self.check_sandbox_paths()?;
        self.check_slot_directory()?;
        // Harbor's inhibition, the publisher pause and the opening GC root
        // write these as root, by pathname: only root may modify them.
        let startup = Path::new(super::string(
            &self.maintenance["startup_inhibition"],
            "state_dir",
        )?);
        let writable_by_root = "a directory only root can write";
        self.check_root_directory(startup, 0o022, writable_by_root)?;
        self.check_root_directory(&self.drop_in_root()?, 0o022, writable_by_root)?;
        self.check_root_directory(&self.gcroots, 0o022, writable_by_root)?;
        self.refuse_stale_evidence(journal)?;
        // The socket, port and identifier `inspect-fence` uses once the fence
        // is open, checked against the running primary first (one already
        // down has nothing to answer).
        if self.unit_state(self.primary_unit())?.1 == "active" {
            self.inspect_live()?;
        }
        let authority = Path::new(super::string(&self.maintenance, "state_dir")?);
        self.provision(authority, "postgres", 0o700)?;
        // Harbor would create missing parents of its inhibition state private
        // to root, where a sibling postgres path could no longer be searched.
        self.walk(startup, &[], Some(0o700))?;
        self.inhibit(journal)?;
        self.pause_publishers(journal)?;
        self.stop_clients(journal)?;
        self.stop_primary(journal)?;
        self.open_fence(journal, adopt)?;
        self.release_startup(journal, "prepared")?;
        self.start_primary(true)?;
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
        if self.export_sandboxed() {
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
        let restarted = self.restart(journal, |journal| &mut journal.pending_receivers);
        self.save(journal)?;
        restarted
    }

    /// Start every unit of the journal's `list`. Each one started (or not
    /// loaded by the running generation, then moved to `unloaded`) leaves the
    /// list and is journaled at once, so a retry never starts a finished
    /// oneshot again; those that fail stay listed for the next retry.
    fn restart(
        &self,
        journal: &mut Journal,
        list: fn(&mut Journal) -> &mut Vec<String>,
    ) -> Result<()> {
        let mut first = None;
        for unit in list(journal).clone() {
            match self.start_queued(&unit) {
                Ok(loaded) => {
                    list(journal).retain(|listed| *listed != unit);
                    if !loaded && !journal.unloaded.contains(&unit) {
                        journal.unloaded.push(unit);
                    }
                    self.save(journal)?;
                }
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Wait for the worker, stopping its unit on an interrupt or the deadline.
    fn await_worker(&self, import: bool, signals: &Deferred) -> Result<bool> {
        let command = self.preparation_command(import);
        self.await_unit(
            command,
            PREPARE_UNIT,
            PREPARE_TIMEOUT,
            "preparation",
            signals,
        )
    }

    /// Wait for a transient unit's `systemd-run --wait` client, stopping the
    /// unit on an interrupt or the deadline.
    fn await_unit(
        &self,
        mut command: Command,
        unit: &str,
        timeout: Duration,
        what: &str,
        signals: &Deferred,
    ) -> Result<bool> {
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
        let deadline = Instant::now() + timeout + Duration::from_secs(60);
        loop {
            let exited = child.try_wait()?;
            let interrupted = signals.pending();
            if interrupted || (exited.is_none() && Instant::now() >= deadline) {
                // The unit may only now be starting: stop it until its client
                // returns. A unit that outlives this stays `BUSY` for receivers.
                for _ in 0..60 {
                    let _ = self.systemctl(&["stop", "--", unit]);
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(invalid(if interrupted {
                    format!("{what} interrupted; its worker was stopped")
                } else {
                    format!("{what} exceeded its execution limit; its worker was stopped")
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
    fn run_preparation(
        &self,
        journal: &mut Journal,
        import: bool,
        settle: impl FnOnce(&mut Journal, bool) -> Result<()>,
    ) -> Result<()> {
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        self.preflight_paths()?;
        self.provision_all()?;
        self.check_sandbox_paths()?;
        let signals = Deferred::block()?;
        let quiesced = (|| {
            for unit in &self.manifest.receiver_units {
                // A transitional receiver can still be writing WAL; quiesce every
                // non-terminal state (deactivating may be a restart), and resume
                // every one it stopped.
                if restorable(&self.show(unit, "ActiveState")?) {
                    if !journal.pending_receivers.contains(unit) {
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
        // Journal the worker's outcome (its phase, export or admission) before
        // receiver cleanup: a cleanup retry must never rerun preparation and
        // its export command. Pending receivers stay journaled for the retry.
        let settled =
            (self.await_worker(import, &signals)).and_then(|ready| settle(journal, ready));
        let restarted = self.restart_receivers(journal);
        settled.and(restarted)
    }

    /// The hand-off export the postgres-written evidence names: one repository
    /// identifier directly below the export root, reached and read without
    /// following a symlink, so no postgres-side writer can point the journal
    /// or the hand-off at another directory.
    fn reported_export(&self, preparation: &Value) -> Result<(PathBuf, String)> {
        self.export_plan(super::string(preparation, "backup_id")?)
    }

    /// The export `backup` names below the export root and its plan's digest.
    fn export_plan(&self, backup: &str) -> Result<(PathBuf, String)> {
        let root = &self.manifest.prepare.export_root;
        let export = root.join(backup);
        if !super::recovery_repository::valid_identifier(backup)
            || export.parent() != Some(root.as_path())
        {
            return Err(invalid(
                "preparation reported an export outside the configured export root",
            ));
        }
        let unavailable = || {
            invalid("recovery preparation failed; retain the writer window and inspect its unit")
        };
        let (directory, _) = (self.walk(
            &export,
            &[self.writable_roots(), vec![root.as_path()]].concat(),
            None,
        )?)
        .ok_or_else(unavailable)?;
        let mut plan = open_at(
            &directory,
            "recovery-export.json".as_ref(),
            libc::O_RDONLY | libc::O_NONBLOCK,
        )
        .map_err(|_| unavailable())?;
        if !plan.metadata()?.is_file() {
            return Err(unavailable());
        }
        Ok((export, codec::hash_reader(&mut plan)?))
    }

    fn prepare(&self, journal: &mut Journal) -> Result<()> {
        self.inspect_fence(journal)?;
        let import = self.import_slot(journal)?;
        if self.recovered_outcome(journal, import)? {
            return Ok(());
        }
        self.run_preparation(journal, import, |journal, ready| {
            if ready {
                return self.settle_imported(journal);
            }
            if import {
                return Err(invalid(
                    "recovery preparation failed; retain the writer window and inspect its unit",
                ));
            }
            self.settle_prepared(journal)
        })
    }

    /// The outcome of a worker that finished before it was journaled (the
    /// coordinator was killed in between), recovered from Harbor's own
    /// evidence, so preparation and its non-idempotent export never run twice
    /// in one writer window. Admission is what a ready worker leaves; a
    /// fence-bound snapshot, its preparation binding and the export plan are
    /// what one stopped at the certification hand-off leaves.
    fn recovered_outcome(&self, journal: &mut Journal, import: bool) -> Result<bool> {
        if import {
            if !self.admitted() {
                return Ok(false);
            }
            self.settle_imported(journal)?;
        } else if self.settle_prepared(journal).is_err() {
            return Ok(false);
        }
        Ok(true)
    }

    /// Whether Harbor admits the recovery: the read-only check a ready
    /// preparation worker ends with.
    fn admitted(&self) -> bool {
        self.response(true, &["inspect-recovery"], "ready").is_ok()
    }

    fn settle_imported(&self, journal: &mut Journal) -> Result<()> {
        // The import's preparation reruns the export command: admission is
        // recorded only over the certified hand-off, its plan unchanged.
        if let (Some(export), Some(expected)) = (&journal.export, &journal.export_sha256) {
            let backup = (export.file_name().and_then(|name| name.to_str()))
                .ok_or_else(|| invalid("the recorded export has no name"))?;
            if !(self.export_plan(backup)).is_ok_and(|(_, digest)| digest == *expected) {
                return Err(invalid(
                    "the exporter rewrote the certified hand-off while the receipt was imported; the writer window is retained: restore the certified export and rerun import, or abort",
                ));
            }
        }
        journal.phase = Phase::Imported;
        self.save(journal)
    }

    /// Harbor stops before admission until the independent receipt exists;
    /// the retained export, under evidence bound to this fence, is then the
    /// hand-off to the executor.
    fn settle_prepared(&self, journal: &mut Journal) -> Result<()> {
        // Postgres-written evidence: read pinned and bounded.
        let evidence = self.evidence_dir()?.join("preparation.json");
        let preparation = codec::decode(&read_pinned(&evidence, MAX_RECORD_BYTES)?.1)?;
        self.require_bound_evidence(journal)?;
        let (export, digest) = self.reported_export(&preparation)?;
        journal.export_sha256 = Some(digest);
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
        let (expected, bytes) = verified_receipt(receipt, expected)?;
        // Stage the verified bytes, never a pathname the transport can still swap.
        // An interrupted import may already have staged these exact bytes:
        // reuse them. A recorded receipt that failed admission is set aside
        // for the new one; a slot that holds anything else is never replaced.
        let slot = &self.manifest.import_slot;
        // Staging needs a root-private directory under root-owned ancestry,
        // never one created on the root filesystem below an absent mount.
        let directory = slot
            .parent()
            .ok_or_else(|| invalid("import slot has no directory"))?;
        self.require_covering_mounts(directory)?;
        self.walk(directory, &[], Some(0o700))?;
        if let Ok(metadata) = fs::symlink_metadata(slot) {
            trusted_file(&metadata, slot, euid())?;
            let staged = read_pinned(slot, super::recovery::MAX_OFF_HOST_RECEIPT_BYTES)?.1;
            let digest = codec::digest(&staged);
            if digest != expected {
                if journal.receipt_sha256.as_deref() != Some(digest.as_str()) {
                    return Err(invalid(
                        "import slot holds a different, unrecorded receipt; retain the writer window",
                    ));
                }
                self.set_aside(&staged)?;
                fs::remove_file(slot)?;
                durable::sync_directory(directory)?;
            }
        }
        // Recorded before staging, so the finished rollout retires exactly it.
        journal.receipt_sha256 = Some(expected);
        self.save(journal)?;
        if fs::symlink_metadata(slot).is_err() {
            let verified = self.state.join("receipt.verified.json");
            durable::atomic_write_mode(&verified, &bytes, 0o600)?;
            super::recovery::stage_off_host_import(&verified, slot)?;
        }
        self.inspect_fence(journal)?;
        if self.recovered_outcome(journal, true)? {
            return Ok(());
        }
        self.run_preparation(journal, true, |journal, ready| {
            if !ready {
                return Err(invalid(
                    "independent receipt was not admitted; retain the writer window",
                ));
            }
            self.settle_imported(journal)
        })
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
    fn reopen(&self, journal: &mut Journal, adopt: Option<&str>) -> Result<()> {
        // Before any side effect: never close the fence or start a writer under
        // a preparation worker that is still copying.
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        if !journal.reopened {
            // Killed between `inhibit-startup` and saving its token: the held
            // inhibition carries it.
            if journal.inhibiting && journal.startup_token.is_none() {
                journal.startup_token = self.held_inhibition()?;
                self.save(journal)?;
            }
            if let (None, Some(startup)) = (&journal.fence_token, journal.startup_token.clone()) {
                // Interrupted between inhibition and fence-open. Harbor binds the
                // startup release to a fence boundary, so complete the offline
                // acquisition first and then unwind it through the normal close.
                let live = self.live_fence()?;
                if self.held_inhibition()?.as_deref() != Some(startup.as_str()) {
                    // The shared inhibition was already released at another
                    // coordinator's fence boundary: only restart the primary
                    // this rollout stopped (an unrecorded fence is not ours to verify).
                    self.start_primary(false)?;
                    if live.is_none() {
                        self.inspect_live()?;
                    }
                } else if let Some(foreign) = live.filter(|_| !journal.fence_opening) {
                    return Err(invalid(format!(
                        "this rollout holds startup inhibition {startup} but Harbor holds writer fence \
                         {foreign}, which it did not open; Harbor releases startup only at a fence \
                         boundary, so finish fence {foreign} with the coordinator that opened it (that \
                         releases the shared inhibition), then rerun `rollout abort`"
                    )));
                } else {
                    // Refuse an unadopted fence before touching the primary.
                    self.refuse_foreign_fence(journal, adopt)?;
                    self.stop_primary(journal)?;
                    self.open_fence(journal, adopt)?;
                    self.release_startup(journal, "prepared")?;
                }
            }
            if let Some(token) = journal.fence_token.clone() {
                // Declared clients whose start the fence's condition skipped (one
                // the candidate introduced, or a timer that fired meanwhile) are
                // restored with the stopped ones; recorded before the fence (and
                // the marker telling when it began) is gone.
                let marker = writer_fence::marker(&self.maintenance)?;
                if let Ok(fenced) = fs::symlink_metadata(&marker) {
                    for (unit, _) in self.services(true)? {
                        if !journal.clients.contains(&unit)
                            && (self.manifest.clients.iter())
                                .any(|pattern| unit_matches(pattern, &unit))
                            && self.skipped_since(&unit, fenced.mtime())?
                        {
                            journal.clients.push(unit);
                            self.save(journal)?;
                        }
                    }
                }
                self.inhibit(journal)?;
                self.stop_primary(journal)?;
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
                self.start_primary(false)?;
                self.inspect_live()?;
            }
            // Only the opening manifest unwinds, so its retirement policy
            // decides which recorded clients this unwind restarts.
            journal.pending_clients = (journal.clients.iter())
                .filter(|unit| {
                    !(self.manifest.retired_clients.iter())
                        .any(|pattern| unit_matches(pattern, unit))
                })
                .cloned()
                .collect();
            journal.reopened = true;
            self.save(journal)?;
        }
        // Every client, receiver and publisher is tried before the first
        // failure is reported: one failing client must not keep the rest down.
        let started = self.restart(journal, |journal| &mut journal.pending_clients);
        let receivers = self.restart_receivers(journal);
        let lifted = self.lift_pause(journal);
        started.and(receivers).and(lifted)
    }

    /// Whether the generation whose rollout manifest `path` names carries
    /// exactly this one.
    fn carries(&self, path: &Path) -> Result<bool> {
        Ok(fs::canonicalize(path).ok() == Some(fs::canonicalize(&self.config)?))
    }

    /// `activated` and `thaw` act on the deployed generation, so it must carry
    /// exactly this manifest.
    fn require_installed(&self) -> Result<()> {
        if !self.carries(&self.installed)? {
            return Err(invalid(format!(
                "the running generation's {} is not this manifest; activate the candidate first",
                self.installed.display()
            )));
        }
        Ok(())
    }

    /// Where `accept` writes the deployment acceptance: the root-private
    /// journal root.
    fn acceptance_path(&self) -> PathBuf {
        self.state.join("acceptance.json")
    }

    /// The deployment acceptance thaw checks, from the journal: bound to the
    /// host, primary, manifest, fence and record snapshot; written root 0600.
    fn accept(&self, journal: &Journal) -> Result<()> {
        let snapshot = Path::new(super::string(
            &self.maintenance["recovery"],
            "snapshot_file",
        )?);
        let acceptance = json!({
            "version": 1,
            "status": "accepted",
            "host": self.manifest.host,
            "system_identifier": self.manifest.system_identifier,
            "manifest_sha256": self.digest,
            "token": journal.fence_token,
            "snapshot_sha256": codec::file_digest(snapshot)?,
        });
        let bytes = serde_json::to_vec(&acceptance)?;
        durable::atomic_write_mode(&self.acceptance_path(), &bytes, 0o600)
    }

    /// The acceptance must bind this host, primary, manifest, fence and
    /// record snapshot.
    fn check_acceptance(&self, journal: &Journal, accepted: &Value) -> Result<()> {
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
        Ok(())
    }

    fn thaw(&self, journal: &mut Journal, acceptance: &Path) -> Result<()> {
        // Validate, digest and parse one pinned descriptor, never a pathname
        // that could be swapped (or a FIFO opened) between checks and reads.
        let not_root_owned = || invalid("acceptance must be a root-owned mode-0600 regular file");
        let (metadata, bytes) =
            read_pinned(acceptance, MAX_RECORD_BYTES).map_err(|_| not_root_owned())?;
        if metadata.uid() != 0 {
            return Err(not_root_owned());
        }
        trusted_file(&metadata, acceptance, 0).map_err(|_| not_root_owned())?;
        let digest = codec::digest(&bytes);
        self.check_acceptance(journal, &codec::decode(&bytes)?)?;
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
        self.reopen(journal, None)?;
        self.retire_slot(journal)?;
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
        // Digested here, on the executor, so `import` checks the transport.
        let receipt = workspace.join("off-host-recovery.json");
        let receipt_sha256 = codec::file_digest(&receipt)?;
        Ok(json!({
            "status": "certified",
            "receipt": receipt,
            "receipt_sha256": receipt_sha256,
            "next": format!(
                "transport the receipt to the primary as a private file, then run \
                 `rollout import --receipt <transported receipt> --sha256 {receipt_sha256}`"
            ),
        }))
    }

    fn next_hint(&self, journal: &Journal) -> Value {
        let config = self.config.display();
        let executor = &self.manifest.executor;
        // Unwinding (reopened, unfinished): only the unwind itself continues.
        if journal.reopened && !matches!(journal.phase, Phase::Thawed | Phase::Aborted) {
            // Thaw needs the candidate installed, abort needs it gone.
            return json!(
                if journal.phase == Phase::Activated && self.require_installed().is_ok() {
                    "the thaw is unwinding; rerun `rollout thaw` with the same acceptance"
                } else {
                    "the abort is unwinding; rerun `rollout abort`"
                }
            );
        }
        let acceptance = self.acceptance_path();
        let max_age = (self.maintenance["recovery"]["max_age_seconds"].as_u64()).map_or_else(
            || "its maxAgeSeconds".into(),
            |seconds| format!("{seconds}s"),
        );
        match journal.phase {
            Phase::Prepared if executor.transport.is_some() && journal.receipt_sha256.is_some() => {
                json!(
                    "the recorded receipt awaits admission: `rollout next` retries it (inspect \
                     harbor-db-rollout-prepare.service if it is refused); to certify again, run \
                     `rollout certify` on the executor and `rollout import` here, which sets the \
                     recorded receipt aside"
                )
            }
            Phase::Prepared if executor.transport.is_some() => {
                json!("run `rollout next`; the executor transport hook certifies and imports")
            }
            // The executor needs this manifest's store closure (Harbor and the
            // certify argv it names) as well as the export itself.
            Phase::Prepared => json!(format!(
                "copy the manifest closure to {host}: `nix copy --to ssh-ng://{host} {config}`; \
                 transfer {export} to {host} as a directory only {user} can read; then on {host} as {user}: \
                 {harbor} --config {config} rollout certify --export <copied export> --export-sha256 {sha} \
                 --workspace <fresh dir> (the export's SHA-256; the executor command must write its \
                 receipt to <workspace>/off-host-recovery.json); then here: `rollout import --receipt \
                 <transported receipt> --sha256 <receipt_sha256 printed by certify>`",
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
            Phase::Activated => json!(format!(
                "verify the deployment, then run `rollout accept` (it writes {acceptance}) and `rollout thaw --acceptance {acceptance}`; if verification fails, or the recovery evidence ages past {max_age} after `prepare` so thaw cannot re-admit it, roll back to the previous generation as the boot default (`nixos-rebuild switch --rollback`), then run `rollout abort`",
                acceptance = acceptance.display()
            )),
            Phase::Thawed => json!(format!(
                "thawed; within {max_age} of `prepare`, retire the rollout: activate a generation without `rollout` (and `recoveryPreparation`) and with `recovery.requireWriterFence` false, or the consumer's equivalent (one still declaring `rollout` cannot activate once the fence is closed); then `rollout archive`"
            )),
            Phase::Aborted => json!(
                "aborted; `rollout archive` retires this journal; a new rollout starts only with an explicit `rollout fence`"
            ),
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
        if self.transporting()? {
            value["transport_hook"] = json!(TRANSPORT_UNIT);
            value["next"] = json!(TRANSPORT_BUSY);
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
    /// adoption, or was adopted under another rollout manifest. (A generation
    /// without the coordinator may roll an adopted one back; abort then
    /// unwinds it.) Recovery admission (`inspect-recovery`) still gates the
    /// switch separately.
    fn activation_check(&self) -> Result<Value> {
        if let Some(journal) = self.journal()? {
            stranding(&journal, Some(&self.digest))?;
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
                    && [".aborted.json", ".thawed.json", ".receipt.json"]
                        .iter()
                        .any(|kind| name.ends_with(kind))
            })
            .collect();
        names.sort();
        Ok(names)
    }

    /// At a terminal transition the opening manifest moves a staged receipt
    /// out of its import slot into the journal root, so the slot is free for
    /// the next rollout and archival needs no manifest. Only a private root
    /// file holding exactly the recorded receipt moves. Written before the slot
    /// is removed, so an interrupted move is simply repeated.
    fn retire_slot(&self, journal: &Journal) -> Result<()> {
        let Some(recorded) = journal.receipt_sha256.as_deref() else {
            return Ok(());
        };
        let slot = &self.manifest.import_slot;
        let metadata = match fs::symlink_metadata(slot) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(metadata) => metadata,
        };
        trusted_file(&metadata, slot, euid())?;
        let bytes = read_pinned(slot, super::recovery::MAX_OFF_HOST_RECEIPT_BYTES)?.1;
        if codec::digest(&bytes) == recorded {
            durable::atomic_write_mode(&self.state.join(RETIRED_RECEIPT), &bytes, 0o600)?;
        } else {
            // Not what was recorded (an older staging left it truncated): keep
            // it for inspection, but never let it hold the slot.
            self.set_aside(&bytes)?;
        }
        fs::remove_file(slot)?;
        durable::sync_directory(slot.parent().unwrap_or(Path::new("/")))
    }

    /// Keep receipt bytes that no longer hold the slot beside the journal,
    /// as the next free `receipt.rejected.<n>.json`, never replacing one.
    fn set_aside(&self, bytes: &[u8]) -> Result<PathBuf> {
        let staging = self
            .state
            .join(format!(".receipt.rejected.{}", std::process::id()));
        durable::atomic_write_mode(&staging, bytes, 0o600)?;
        let mut kept = None;
        for n in 1.. {
            let target = self.state.join(format!("receipt.rejected.{n}.json"));
            match fs::hard_link(&staging, &target) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                linked => {
                    kept = Some(linked.map(|()| target));
                    break;
                }
            }
        }
        let _ = fs::remove_file(&staging);
        Ok(kept.expect("an unbounded search ends")?)
    }

    /// A finished rollout (thawed, or aborted and fully unwound) is archived so
    /// a new one can start: a thawed one by any manifest (nothing is open), an
    /// aborted one only by its opener. An unwind that is unrecorded or under a
    /// held fence never is.
    fn archive_finished(&self) -> Result<()> {
        let Some(journal) = self.journal()? else {
            return Ok(());
        };
        let kind = match journal.phase {
            Phase::Thawed => "thawed",
            Phase::Aborted => {
                // Fully unwound with its receipt (if any) already retired, an
                // abort holds nothing of its opener's: any manifest archives it.
                // Older journals, whose slot was never retired, need the opener.
                let retired = journal.receipt_sha256.is_none()
                    || writer_fence::exists(&self.state.join(RETIRED_RECEIPT));
                if !(journal.reopened && retired) {
                    self.require_opener(&journal)?;
                }
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
                if !retired {
                    self.retire_slot(&journal)?;
                }
                "aborted"
            }
            _ => return Ok(()),
        };
        // One collision-free id for the receipt and journal of an archive:
        // the next sequence number, published with no-replace hard links (a
        // taken name fails with EEXIST and the next is tried), so nothing is
        // ever overwritten. A retry after a crash takes a new id.
        let receipt = self.state.join(RETIRED_RECEIPT);
        let has_receipt = writer_fence::exists(&receipt);
        let mut id = (self.archives()?.iter())
            .filter_map(|name| name.split('.').nth(1)?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        loop {
            id += 1;
            let archived = |name: &str| self.state.join(format!("rollout.{id}.{name}.json"));
            if has_receipt {
                match fs::hard_link(&receipt, archived("receipt")) {
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    other => other?,
                }
            }
            match fs::hard_link(self.journal_path(), archived(kind)) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if has_receipt {
                        fs::remove_file(archived("receipt"))?;
                    }
                }
                other => break other?,
            }
        }
        if has_receipt {
            fs::remove_file(&receipt)?;
        }
        fs::remove_file(self.journal_path())?;
        durable::sync_directory(&self.state)?;
        // Its opening manifest no longer needs to survive GC.
        match fs::remove_file(self.gcroots.join(GC_ROOT)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            other => other?,
        }
        durable::sync_directory(&self.gcroots)
    }

    fn execution_lease(&self) -> Result<Option<durable::Lease>> {
        let Some(path) = &self.manifest.execution_lock else {
            return Ok(None);
        };
        // The lock serializes fencing with activation only if no one else can
        // swap its inode: it must sit in a real directory chain only root can
        // modify (no symlink, no writable or sticky-shared directory), and the
        // inode opened without following a symlink is the one locked.
        let parent = path
            .parent()
            .ok_or_else(|| invalid("execution lock has no directory"))?;
        match self.walk(parent, &[], None)? {
            Some((_, metadata))
                if [0, euid()].contains(&metadata.uid()) && metadata.mode() & 0o022 == 0 => {}
            _ => {
                return Err(invalid(format!(
                    "execution lock {} must sit in a directory only root can modify",
                    path.display()
                )));
            }
        }
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

    fn abort(
        &self,
        adopt_fence_token: Option<String>,
        adopt_startup_token: Option<String>,
    ) -> Result<Value> {
        let mut adopted = false;
        let mut opening = None;
        let retained = self.journal()?;
        if let Some(journal) = &retained {
            self.require_opener(journal)?;
        }
        if let Some(token) = adopt_startup_token {
            return self.abort_inhibition(retained.is_some(), &token);
        }
        let mut journal = match (retained, adopt_fence_token) {
            (Some(journal), None) => journal,
            (Some(journal), Some(token)) => {
                Self::adoptable(&journal, Some(&token)).map_err(|_| {
                    invalid(
                        "a rollout journal records its fence; abort it without --adopt-fence-token",
                    )
                })?;
                opening = Some(token);
                journal
            }
            (None, None) => {
                return Err(invalid(
                    match (self.live_fence()?, self.held_inhibition()?) {
                        (Some(token), _) => format!(
                            "no rollout journal, but Harbor holds writer fence {token} ({}); verify it with \
                         `inspect-fence --token {token}` and, if no other coordinator owns it, rerun \
                         `rollout abort --adopt-fence-token {token}`",
                            writer_fence::marker(&self.maintenance)?.display()
                        ),
                        (None, Some(token)) => format!(
                            "no rollout journal, but Harbor holds startup inhibition {token}, which keeps \
                         the primary from starting; if no other coordinator owns it, rerun `rollout \
                         abort --adopt-startup-token {token}`"
                        ),
                        (None, None) => "no rollout to abort".into(),
                    },
                ));
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
                adopted = true;
                journal
            }
        };
        if journal.phase == Phase::Thawed {
            return Err(invalid("the rollout is thawed; nothing to abort"));
        }
        // An installed candidate (adopted, activated) would run unfenced
        // without the thaw's acceptance and cutover checks. Once the operator
        // rolled back (a failed verification, or evidence aged past what thaw
        // re-admits), it unwinds.
        if matches!(journal.phase, Phase::Adopted | Phase::Activated)
            && self.require_installed().is_ok()
        {
            return Err(invalid(if journal.phase == Phase::Adopted {
                "the candidate is activated; run `rollout activated`, then thaw with an acceptance (or roll back to the previous generation, then abort)"
            } else {
                "the candidate is activated; thaw with an acceptance, or roll back to the previous generation, then abort"
            }));
        }
        // Rolled back for now only (`nixos-rebuild test --rollback`), the
        // candidate would boot next, unfenced and without thaw's checks.
        if matches!(journal.phase, Phase::Adopted | Phase::Activated) {
            if self.carries(&self.profile)? {
                return Err(invalid(
                    "the candidate is still the boot default; make the rolled-back generation the boot default first (`nixos-rebuild switch --rollback`, or `nixos-rebuild boot --rollback`), then abort",
                ));
            }
            if self.carries(&self.booted)? {
                return Err(invalid(
                    "the host booted the candidate; boot the rolled-back generation first, then abort",
                ));
            }
        }
        if journal.phase != Phase::Aborted {
            self.reopen(&mut journal, opening.as_deref())?;
            self.retire_slot(&journal)?;
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

    /// Journal lost while only Harbor's startup inhibition is held (no fence):
    /// adopt it explicitly and unwind it through the normal pre-fence path,
    /// which completes the fence boundary Harbor releases startup at.
    fn abort_inhibition(&self, journaled: bool, token: &str) -> Result<Value> {
        if journaled {
            return Err(invalid(
                "a rollout journal exists; abort it without --adopt-startup-token",
            ));
        }
        if self.live_fence()?.is_some() {
            return Err(invalid(
                "Harbor holds a writer fence; adopt it with --adopt-fence-token instead",
            ));
        }
        if self.held_inhibition()?.as_deref() != Some(token) {
            return Err(invalid(
                "--adopt-startup-token differs from Harbor's held startup inhibition",
            ));
        }
        let mut journal = self.fresh_journal();
        journal.inhibiting = true;
        journal.startup_token = Some(token.to_owned());
        journal.pending_receivers = self.manifest.receiver_units.clone();
        journal.stopped_publishers = self.manifest.paused_publishers.clone();
        self.save(&journal)?;
        self.reopen(&mut journal, None)?;
        journal.phase = Phase::Aborted;
        self.save(&journal)?;
        let mut status = self.status()?;
        status["next"] = json!(
            "restart the writer clients the lost journal had stopped (re-activate the current generation)"
        );
        Ok(status)
    }

    /// Read-only actions and the executor's `certify` are answered here;
    /// only steps reach `run` (whose arm for these is therefore unreachable).
    fn dispatch(&self, action: Action) -> Result<Value> {
        match action {
            Action::Status => self.status(),
            Action::ActivationCheck => self.activation_check(),
            Action::Certify {
                export,
                export_sha256,
                workspace,
            } => self.certify(&export, &export_sha256, &workspace),
            action => {
                if !root() {
                    return Err(invalid("rollout steps require root on the primary host"));
                }
                if fs::read_to_string("/proc/sys/kernel/hostname")?.trim() != self.manifest.host {
                    return Err(invalid("rollout must run on its declared primary host"));
                }
                self.run(action)
            }
        }
    }

    pub fn run(&self, action: Action) -> Result<Value> {
        self.state_dir()?;
        let _lease = durable::lock(&self.state.join("rollout.lock"), false, true)?;
        let _execution = self.execution_lease()?;
        // An orphaned preparation worker may still be copying under the fence:
        // no step (unwinding included) starts until it is gone.
        if self.preparing()? {
            return Err(invalid(BUSY));
        }
        // Likewise an orphaned transport hook may still be reading the export.
        if self.transporting()? {
            return Err(invalid(TRANSPORT_BUSY));
        }
        if let Action::Abort {
            adopt_fence_token,
            adopt_startup_token,
        } = action
        {
            return self.abort(adopt_fence_token, adopt_startup_token);
        }
        // Archiving retires a finished journal (and its GC root) without
        // fencing; only an explicit `fence` starts a new rollout after one.
        if let Action::Archive = action {
            let phase = self.journal()?.map(|journal| journal.phase);
            if let Some(phase) = phase.filter(|p| !matches!(p, Phase::Thawed | Phase::Aborted)) {
                return Err(invalid(format!(
                    "the rollout is {phase:?}, not finished; nothing to archive"
                )));
            }
            self.archive_finished()?;
            return self.status();
        }
        if matches!(action, Action::Fence { .. }) {
            self.archive_finished()?;
        }
        let mut journal = self.bound()?;
        if let Action::Fence { adopt_fence_token } | Action::Next { adopt_fence_token } = &action {
            Self::adoptable(&journal, adopt_fence_token.as_deref())?;
        }
        let unwinding =
            journal.reopened && !matches!(journal.phase, Phase::Aborted | Phase::Thawed);
        if unwinding && !matches!(action, Action::Thaw { .. }) {
            return Err(invalid(
                "the rollout is unwinding; rerun `rollout abort` or `rollout thaw`",
            ));
        }
        // A step that journaled its outcome but failed restoring receivers left
        // them pending: any retry, in whatever phase it now is, resumes them.
        if !journal.pending_receivers.is_empty() {
            self.restart_receivers(&mut journal)?;
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
            Action::Fence { adopt_fence_token } => {
                require(&[Phase::Idle, Phase::Fenced])?;
                self.fence(&mut journal, adopt_fence_token.as_deref())?;
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
            Action::Accept => {
                require(&[Phase::Activated])?;
                self.require_installed()?;
                self.accept(&journal)?;
            }
            Action::Thaw { acceptance } => {
                require(&[Phase::Activated, Phase::Thawed])?;
                if phase == Phase::Activated {
                    self.require_installed()?;
                    self.thaw(&mut journal, &acceptance)?;
                }
            }
            Action::Next { adopt_fence_token } => match phase {
                Phase::Idle => self.fence(&mut journal, adopt_fence_token.as_deref())?,
                Phase::Fenced => self.prepare(&mut journal)?,
                Phase::Prepared if self.manifest.executor.transport.is_some() => {
                    self.transport(&mut journal)?
                }
                // External hand-off: certification runs on the executor; `next`
                // prints the exact commands.
                Phase::Prepared => {}
                Phase::Imported => self.adopt(&mut journal)?,
                _ => {}
            },
            Action::Status
            | Action::ActivationCheck
            | Action::Certify { .. }
            | Action::Abort { .. }
            | Action::Archive => unreachable!(),
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
#[path = "postgres_rollout_transport.rs"]
mod transport;

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    pub(super) fn executable(path: &Path, script: &str) {
        fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// An executable regular store file from PATH (coreutils' `true`).
    pub(super) fn store_tool() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|directory| directory.starts_with("/nix/store"))
            .map(|directory| directory.join("true"))
            .find(|path| store_executable(path))
            .expect("an executable store `true` on PATH")
    }

    /// A regular store file without execute permission, from a PATH package.
    fn store_data_file() -> PathBuf {
        fn find(directory: &Path, depth: u32) -> Option<PathBuf> {
            let mut entries: Vec<_> = fs::read_dir(directory)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .collect();
            entries.sort();
            entries.into_iter().find_map(|path| {
                let metadata = fs::symlink_metadata(&path).ok()?;
                if metadata.is_file() && metadata.mode() & 0o111 == 0 {
                    Some(path)
                } else if metadata.is_dir() && depth > 0 {
                    find(&path, depth - 1)
                } else {
                    None
                }
            })
        }
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|directory| directory.starts_with("/nix/store"))
            .filter_map(|directory| find(directory.parent()?, 3))
            .find(|path| durable::immutable_store_reference(path).is_ok())
            .expect("a non-executable store file in a PATH package")
    }

    pub(super) struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        /// Fake systemd keeps one state file per unit; `setup.service` stays
        /// busy for `polls` show queries after every primary start.
        pub(super) fn new(polls: u32) -> Self {
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
                "gcroots",
                "export",
            ] {
                fs::create_dir(p(dir)).unwrap();
                fs::set_permissions(p(dir), fs::Permissions::from_mode(0o755)).unwrap();
            }
            // Private like the journal; the identity state as tmpfiles keeps it.
            for dir in ["state", "authority"] {
                fs::set_permissions(p(dir), fs::Permissions::from_mode(0o700)).unwrap();
            }
            for unit in [
                "a.service",
                "b.service",
                "postgresql.service",
                "pub.service",
            ] {
                fs::write(p("units").join(unit), "active").unwrap();
            }
            fs::write(p("units/setup.service"), "inactive").unwrap();
            let log = p("calls.log");
            executable(
                &p("systemctl"),
                &format!(
                    r#"u={units}; log={log}
echo "systemctl $*" >> "$log"
[ "$1" = --no-block ] && shift
case "$1" in
  list-units) case "$*" in *--all*) all=1 ;; *) all= ;; esac
    for f in "$u"/*; do s=$(cat "$f"); [ "$s" = inactive ] && [ -z "$all" ] || echo "$(basename "$f") loaded $s x x"; done ;;
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
    if [ "$4" = --property=LoadState,ConditionResult,ConditionTimestamp ]; then
      if [ -f "$u/$unit" ]; then echo LoadState=loaded; else echo LoadState=not-found; fi
      if [ -f "$u/.condition-$unit" ]; then read -r result at < "$u/.condition-$unit"
        echo "ConditionResult=$result"; echo "ConditionTimestamp=@$at"
      else echo ConditionResult=yes; echo ConditionTimestamp=; fi; exit 0
    fi
    if [ "$3" = --property=RequiredBy,BoundBy,ConsistsOf ]; then
      echo "RequiredBy=$(cat "$u/.deps-$unit" 2>/dev/null)"; echo BoundBy=; echo ConsistsOf=; exit 0
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
  # Live (adopt) inspection is ready; admission only once a worker admitted.
  inspect-recovery) if [ -n "$2" ] || [ -f {admitted} ]; then echo '{{"status":"ready"}}'; else exit 1; fi ;;
  *) echo '{{}}' ;;
esac"#,
                    log = log.display(),
                    marker = p("authority/writer-fence.json").display(),
                    inhibited = p("startup/inhibited.json").display(),
                    admitted = p("evidence/.admitted").display(),
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

        pub(super) fn p(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        fn manifest(&self, name: &str, settle: u64) -> PathBuf {
            let manifest = json!({
                "version": 2, "host": "primary", "system_identifier": "42",
                // Load requires store executables; `load` swaps in the fakes.
                "harbor": store_tool(), "systemctl": store_tool(), "systemd_run": store_tool(),
                "maintenance": self.p("maintenance.json"), "preparation": store_tool(),
                "socket_dir": "/run/postgresql", "port": 5432,
                "clients": ["a.service", "b.service"], "retired_clients": [],
                "setup_units": ["setup.service"], "receiver_units": [],
                "paused_publishers": ["pub.service"], "setup_settle_seconds": settle,
                "prepare": {"export_root": self.p("export"), "read_write_paths": [self.p("evidence")],
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
        pub(super) fn load(&self, path: &Path) -> Result<Rollout> {
            let mut rollout = Rollout::load(path)?;
            rollout.manifest.harbor = self.p("harbor");
            rollout.manifest.systemctl = self.p("systemctl");
            rollout.manifest.systemd_run = self.p("systemd-run");
            // A build sandbox has no postgres or users group.
            rollout.group = |group| match group {
                "postgres" | "users" => Ok(unsafe { libc::getegid() }),
                _ => group_id(group),
            };
            rollout.state = self.p("state");
            rollout.installed = self.p("installed.json");
            // A build sandbox's `/` need not be root-owned.
            rollout.anchor = self.root.path().to_owned();
            rollout.service_owner = Some((euid(), unsafe { libc::getegid() }));
            rollout.gcroots = self.p("gcroots");
            rollout.profile = self.p("profile.json");
            rollout.booted = self.p("booted.json");
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
        let status = f.rollout().run(FENCE).unwrap();
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
                adopt_startup_token: None,
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
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains("did not settle"), "{error}");
        assert!(!f.calls().contains("harbor inspect-fence"));
        let journal = f.rollout().journal().unwrap().unwrap();
        assert_eq!(journal.phase, Phase::Idle);
        assert_eq!(journal.fence_token.as_deref(), Some(TOKEN));
    }

    #[test]
    fn fence_resumes_idempotently_and_steps_stay_bound_to_their_manifest() {
        let f = Fixture::new(0);
        f.rollout().run(FENCE).unwrap();
        fs::write(f.p("units/a.service"), "active").unwrap();
        assert_eq!(f.rollout().run(FENCE).unwrap()["phase"], "fenced");
        assert_eq!(f.unit("a.service"), "inactive");
        assert!(f.rollout().run(Action::Adopt).is_err());

        let other = f.load(&f.manifest("other.json", 5)).unwrap();
        for action in [Action::Prepare, NO_ADOPTION] {
            let error = other.run(action).unwrap_err().to_string();
            assert!(error.contains("opened by"), "{error}");
        }
        // Only the opening manifest unwinds it.
        assert_eq!(f.rollout().run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn activated_rollouts_refuse_abort_and_receipts_are_digest_bound() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
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
                adopt_startup_token: None,
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
        manifest["receiver_units"] = json!(["r.service", "d.service"]);
        fs::write(&path, manifest.to_string()).unwrap();
        fs::write(f.p("units/r.service"), "activating").unwrap();
        // Deactivating may be a restart in flight: it is stopped, so it resumes.
        fs::write(f.p("units/d.service"), "deactivating").unwrap();
        let rollout = f.rollout();
        let export = format!("{}", f.p("export").display());
        let writable = rollout
            .preparation_command(false)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .any(|arg| arg.starts_with("--property=ReadWritePaths=") && arg.contains(&export));
        assert!(writable, "export root must be writable by the worker");
        assert!(!worker_ready(&rollout, &mut rollout.bound().unwrap()).unwrap());
        let calls = f.calls();
        assert!(calls.contains("systemctl stop -- r.service"), "{calls}");
        assert!(calls.contains("start -- r.service"), "{calls}");
        assert_eq!(f.unit("d.service"), "active");
    }

    #[test]
    fn interrupted_import_reuses_its_staged_receipt_but_never_replaces_another() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
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
        assert!(error.contains("unrecorded receipt"), "{error}");
    }

    /// The worker's readiness, settled as `prepare` and `import` would.
    fn worker_ready(rollout: &Rollout, journal: &mut Journal) -> Result<bool> {
        let mut ready = false;
        rollout.run_preparation(journal, false, |_, outcome| {
            ready = outcome;
            Ok(())
        })?;
        Ok(ready)
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
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
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
        assert!(worker_ready(&rollout, &mut rollout.bound().unwrap()).is_err());
        assert!(f.calls().contains("start -- r1.service"), "{}", f.calls());
        assert_eq!(f.unit("r1.service"), "active");
    }

    #[test]
    fn preparation_refuses_an_unrecorded_import_slot() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
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
        let error = rollout.run(FENCE).unwrap_err().to_string();
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
        assert!(worker_ready(&rollout, &mut journal).is_err());
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
                adopt_startup_token: None,
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
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(error.contains("required mount"), "{error}");
        assert!(!f.p("mnt/rollout").exists());
    }

    #[test]
    fn next_hands_off_external_steps_with_exact_commands() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        assert_eq!(rollout.run(NEXT).unwrap()["phase"], "fenced");
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        journal.export = Some(f.p("export/backup-1"));
        journal.export_sha256 = Some("a".repeat(64));
        rollout.save(&journal).unwrap();
        let status = rollout.run(NEXT).unwrap();
        assert_eq!(status["phase"], "prepared");
        let next = status["next"].as_str().unwrap();
        // The export digest goes to certify; import takes the receipt's own.
        assert!(
            next.contains("rollout certify")
                && next.contains(&format!("--export-sha256 {}", "a".repeat(64)))
                && next.matches(&"a".repeat(64)).count() == 1
                && next.contains("--sha256 <receipt_sha256 printed by certify>"),
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
        adopt_startup_token: None,
    };
    const FENCE: Action = Action::Fence {
        adopt_fence_token: None,
    };
    const NEXT: Action = Action::Next {
        adopt_fence_token: None,
    };

    #[test]
    fn a_fence_this_rollout_did_not_open_is_refused_not_adopted() {
        let f = Fixture::new(0);
        let other = "f".repeat(32);
        let marker = f.p("authority/writer-fence.json");
        fs::write(&marker, json!({ "token": other }).to_string()).unwrap();
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
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
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains(&other), "{error}");
        untouched(&f);
        // The adoption flag never overrides a fence this rollout did not try to open.
        let error = f
            .rollout()
            .run(Action::Fence {
                adopt_fence_token: Some(other.clone()),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("applies only"), "{error}");
        untouched(&f);
    }

    /// Killed after recording the opening intent but before the token: Harbor
    /// mints tokens, so a live fence then is adopted only explicitly.
    fn interrupted_opening(f: &Fixture, live: Option<&str>) -> Rollout {
        let rollout = f.rollout();
        let mut journal = rollout.bound().unwrap();
        journal.startup_token = Some(TOKEN.into());
        journal.fence_opening = true;
        rollout.save(&journal).unwrap();
        fs::write(
            f.p("startup/inhibited.json"),
            json!({ "token": TOKEN }).to_string(),
        )
        .unwrap();
        if let Some(token) = live {
            fs::write(
                f.p("authority/writer-fence.json"),
                json!({ "token": token }).to_string(),
            )
            .unwrap();
        }
        rollout
    }

    fn adopting(token: &str, abort: bool) -> Action {
        let adopt_fence_token = Some(token.to_owned());
        if abort {
            Action::Abort {
                adopt_fence_token,
                adopt_startup_token: None,
            }
        } else {
            Action::Fence { adopt_fence_token }
        }
    }

    #[test]
    fn an_interrupted_fence_opening_adopts_a_live_fence_only_explicitly() {
        // Nothing was opened: the retry opens a fresh fence, which is ours.
        let f = Fixture::new(0);
        let rollout = interrupted_opening(&f, None);
        assert_eq!(rollout.run(FENCE).unwrap()["fence_token"], TOKEN);
        for abort in [false, true] {
            let f = Fixture::new(0);
            let rollout = interrupted_opening(&f, Some(TOKEN));
            let refused = |action: Action| rollout.run(action).unwrap_err().to_string();
            let error = refused(if abort { NO_ADOPTION } else { FENCE });
            assert!(
                error.contains(&format!("--adopt-fence-token {TOKEN}"))
                    && error.contains("no other coordinator"),
                "{error}"
            );
            let error = refused(adopting(&"f".repeat(32), abort));
            assert!(error.contains("cannot prove"), "{error}");
            let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
            assert!(!calls.contains("harbor fence-open"), "{calls}");
            // A refusal leaves the primary as it found it.
            assert!(!calls.contains("systemctl stop"), "{calls}");
            let status = rollout.run(adopting(TOKEN, abort)).unwrap();
            if abort {
                assert_eq!(status["phase"], "aborted");
                assert!(f.calls().contains("harbor fence-close"));
            } else {
                assert_eq!(status["fence_token"], TOKEN);
            }
            // A recorded token is never re-adopted.
            let error = refused(adopting(TOKEN, abort));
            assert!(error.contains("adopt-fence-token"), "{error}");
        }
    }

    /// Fake a ready hand-off export that preparation reported as `backup_id`.
    pub(super) fn reported(f: &Fixture, backup_id: &str) {
        fs::write(
            f.p("evidence/preparation.json"),
            json!({ "backup_id": backup_id }).to_string(),
        )
        .unwrap();
        // Harbor's record snapshot: the same backup binding, under this fence.
        fs::write(
            f.p("evidence/records.json"),
            json!({ "backup_id": backup_id, "version": 1, "writer_fence_token": TOKEN })
                .to_string(),
        )
        .unwrap();
        fs::create_dir_all(f.p("export/backup-1")).unwrap();
        fs::write(f.p("export/backup-1/recovery-export.json"), "{}").unwrap();
    }

    #[test]
    fn a_reported_export_is_one_identifier_below_the_export_root() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // A postgres-side writer pointed the evidence elsewhere.
        fs::create_dir_all(f.p("elsewhere")).unwrap();
        fs::write(f.p("elsewhere/recovery-export.json"), "{}").unwrap();
        // `export/..` resolves to a real directory holding a plan, too.
        fs::write(f.p("recovery-export.json"), "{}").unwrap();
        for backup_id in ["../elsewhere", "..", "a/../../elsewhere", "link"] {
            reported(&f, backup_id);
            if backup_id == "link" {
                std::os::unix::fs::symlink(f.p("elsewhere"), f.p("export/link")).ok();
            }
            let error = rollout.run(Action::Prepare).unwrap_err().to_string();
            assert!(
                error.contains("outside the configured") || error.contains("untrusted ancestry"),
                "{backup_id}: {error}"
            );
            let journal = rollout.journal().unwrap().unwrap();
            assert!(journal.phase == Phase::Fenced && journal.export.is_none());
        }
        reported(&f, "backup-1");
        let status = rollout.run(Action::Prepare).unwrap();
        assert_eq!(status["phase"], "prepared");
        assert_eq!(status["export_sha256"], codec::digest(b"{}"));
    }

    /// What `import` leaves: a private staged slot and its recorded receipt.
    fn staged(f: &Fixture, rollout: &Rollout, slot: &str, bytes: &[u8]) {
        let slot = f.p(slot);
        fs::create_dir_all(slot.parent().unwrap()).unwrap();
        fs::set_permissions(slot.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&slot, bytes).unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o400)).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        journal.receipt_sha256 = Some(codec::digest(bytes));
        rollout.save(&journal).unwrap();
    }

    /// Insert a shell line into the fake systemctl, before it reads options.
    fn inject(f: &Fixture, script: &str, line: &str) -> String {
        let original = fs::read_to_string(f.p(script)).unwrap();
        fs::write(
            f.p(script),
            original.replacen(
                "[ \"$1\" = --no-block ] && shift",
                &format!("{line}\n[ \"$1\" = --no-block ] && shift"),
                1,
            ),
        )
        .unwrap();
        original
    }

    #[test]
    fn long_running_starts_are_queued_not_awaited() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // pub.service is an hour-long oneshot: a blocking start never returns
        // within any bound (the fake makes it 30 seconds).
        inject(
            &f,
            "systemctl",
            "[ \"$*\" = \"start -- pub.service\" ] && sleep 30",
        );
        let started = Instant::now();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert!(started.elapsed() < Duration::from_secs(20));
        let calls = f.calls();
        for unit in ["pub.service", "a.service", "b.service"] {
            assert!(
                calls.contains(&format!("--no-block start -- {unit}")),
                "{calls}"
            );
        }
        // A queued start that fails is still reported.
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        inject(
            &f,
            "systemctl",
            "[ \"$*\" = \"--no-block start -- a.service\" ] && { echo failed > \"$u/a.service\"; exit 0; }",
        );
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(error.contains("a.service failed to start"), "{error}");
    }

    #[test]
    fn one_failing_client_never_keeps_the_rest_of_the_unwind_down() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.pending_receivers = vec!["r.service".into()];
        rollout.save(&journal).unwrap();
        fs::write(f.p("units/r.service"), "inactive").unwrap();
        let systemctl = fail_start(&f, "a.service");
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(error.contains("a.service"), "{error}");
        // b, the receiver and the publisher pause were all still handled.
        assert_eq!(f.unit("b.service"), "active");
        assert_eq!(f.unit("r.service"), "active");
        assert!(!f.p("state/paused").exists());
        fs::write(f.p("systemctl"), systemctl).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn a_slot_not_holding_the_recorded_receipt_is_set_aside_not_wedging_abort() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // An older staging left the slot truncated.
        staged(&f, &rollout, "import/receipt.json", br#"{"status":"#);
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.receipt_sha256 = Some(codec::digest(br#"{"status":"ready"}"#));
        rollout.save(&journal).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert!(!f.p("import/receipt.json").exists());
        assert_eq!(
            fs::read(f.p("state/receipt.rejected.1.json")).unwrap(),
            br#"{"status":"#
        );
    }

    #[test]
    fn prepared_evidence_must_be_bound_to_this_rollouts_fence() {
        let f = Fixture::new(0);
        // Evidence an earlier window left is refused before the fence.
        reported(&f, "backup-1");
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains("another writer window"), "{error}");
        assert!(f.rollout().journal().unwrap().is_none());
        fs::remove_file(f.p("evidence/records.json")).unwrap();
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains("preparation.json"), "{error}");
        fs::remove_file(f.p("evidence/preparation.json")).unwrap();
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // Preparation failed under this fence but left the old window's evidence.
        reported(&f, "backup-1");
        let snapshot = f.p("evidence/records.json");
        let mut value = durable::read_json(&snapshot).unwrap();
        value["writer_fence_token"] = json!("f".repeat(32));
        fs::write(&snapshot, value.to_string()).unwrap();
        let error = rollout.run(Action::Prepare).unwrap_err().to_string();
        assert!(error.contains("another writer window"), "{error}");
        // Bound to this fence, but not the snapshot's backup.
        value["writer_fence_token"] = json!(TOKEN);
        value["backup_id"] = json!("backup-0");
        fs::write(&snapshot, value.to_string()).unwrap();
        assert!(rollout.run(Action::Prepare).is_err());
        assert_eq!(rollout.journal().unwrap().unwrap().phase, Phase::Fenced);
        reported(&f, "backup-1");
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "prepared");
    }

    #[test]
    fn an_installed_adopted_candidate_is_thawed_not_aborted() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Adopted;
        rollout.save(&journal).unwrap();
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(error.contains("run `rollout activated`"), "{error}");
        // Not yet installed, an adopted rollout still aborts.
        fs::remove_file(f.p("installed.json")).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
    }

    #[test]
    fn static_failures_of_the_hand_off_are_refused_before_the_fence() {
        let f = Fixture::new(0);
        // A writable path below a directory others can write.
        fs::create_dir_all(f.p("shared/backups")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o777)).unwrap();
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("shared/backups")])
        })
        .unwrap();
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(error.contains("untrusted ancestry"), "{error}");
        f.manifest("rollout.json", 2);
        // An import slot directory staging would refuse.
        fs::create_dir(f.p("import")).unwrap();
        fs::set_permissions(f.p("import"), fs::Permissions::from_mode(0o755)).unwrap();
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains("must be private to root"), "{error}");
        let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
        assert!(
            calls
                .lines()
                .all(|line| line.starts_with("systemctl show ")),
            "{calls}"
        );
        // `import` creates a missing one private before staging.
        fs::remove_dir(f.p("import")).unwrap();
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        rollout.save(&journal).unwrap();
        let receipt = f.p("receipt.json");
        fs::write(&receipt, "{}").unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let _ = rollout.run(Action::Import {
            receipt,
            sha256: codec::digest(b"{}"),
        });
        assert_eq!(fs::metadata(f.p("import")).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn source_local_recovery_is_refused_at_load() {
        let f = Fixture::new(0);
        let mut maintenance: Value =
            serde_json::from_slice(&fs::read(f.p("maintenance.json")).unwrap()).unwrap();
        maintenance["recovery"]["repository_protocol"] = json!("source-local-v1");
        fs::write(f.p("maintenance.json"), maintenance.to_string()).unwrap();
        let error = f.load(&f.p("rollout.json")).err().unwrap().to_string();
        assert!(error.contains("legacy recovery repository"), "{error}");
    }

    #[test]
    fn a_recorded_receipt_that_failed_admission_is_replaced() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let rejected = br#"{"status":"stale"}"#;
        staged(&f, &rollout, "import/receipt.json", rejected);
        let fresh = br#"{"status":"ready"}"#;
        let receipt = f.p("receipt.json");
        fs::write(&receipt, fresh).unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        // Staging itself needs root; the replacement before it does not.
        let _ = rollout.run(Action::Import {
            receipt,
            sha256: codec::digest(fresh),
        });
        assert_eq!(
            fs::read(f.p("state/receipt.rejected.1.json")).unwrap(),
            rejected
        );
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.receipt_sha256, Some(codec::digest(fresh)));
        if !root() {
            assert!(!f.p("import/receipt.json").exists());
        }
    }

    #[test]
    fn publishers_the_running_generation_lacks_are_not_stopped() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["paused_publishers"] = json!(["pub.service", "ghost.service"])
        })
        .unwrap();
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
        let calls = f.calls();
        assert!(!calls.contains("stop -- ghost.service"), "{calls}");
        assert_eq!(f.unit("pub.service"), "inactive");
    }

    #[test]
    fn units_the_primary_stop_takes_down_are_restored() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        // postgresql.target Requires the primary; ml (not a client) and the
        // receiver Require the target; setup is excluded.
        fs::write(
            f.p("units/.deps-postgresql.service"),
            "postgresql.target setup.service",
        )
        .unwrap();
        fs::write(f.p("units/.deps-postgresql.target"), "ml.service r.service").unwrap();
        for unit in ["ml.service", "r.service"] {
            fs::write(f.p("units").join(unit), "active").unwrap();
        }
        rollout.run(FENCE).unwrap();
        let journal = rollout.journal().unwrap().unwrap();
        assert!(
            journal.clients.contains(&"ml.service".to_owned()),
            "{:?}",
            journal.clients
        );
        assert!(!journal.clients.iter().any(|unit| unit.ends_with(".target")));
        assert!(!journal.clients.contains(&"setup.service".to_owned()));
        assert_eq!(journal.pending_receivers, ["r.service"]);
        // The stop took them down with the primary.
        for unit in ["ml.service", "r.service"] {
            fs::write(f.p("units").join(unit), "inactive").unwrap();
        }
        rollout.run(NO_ADOPTION).unwrap();
        assert_eq!(f.unit("ml.service"), "active");
        assert_eq!(f.unit("r.service"), "active");
    }

    #[test]
    fn units_still_deactivating_when_stopped_are_recorded_and_restored() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        let deactivating = |units: &[&str]| {
            for unit in units {
                fs::write(f.p("units").join(unit), "deactivating").unwrap();
            }
        };
        // A receiver caught mid-restart is quiesced for preparation and resumed.
        deactivating(&["r.service"]);
        assert!(worker_ready(&rollout, &mut rollout.bound().unwrap()).is_ok());
        assert!(f.calls().contains("stop -- r.service"), "{}", f.calls());
        assert_eq!(f.unit("r.service"), "active");
        // A hard dependent, receiver, client and publisher still deactivating
        // when the fence starts: the stops complete theirs, so each is recorded.
        fs::write(
            f.p("units/.deps-postgresql.service"),
            "dep.service r.service",
        )
        .unwrap();
        deactivating(&["dep.service", "r.service", "a.service", "pub.service"]);
        rollout.run(FENCE).unwrap();
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.clients, ["a.service", "b.service", "dep.service"]);
        assert_eq!(journal.pending_receivers, ["r.service"]);
        assert_eq!(journal.stopped_publishers, ["pub.service"]);
        rollout.run(NO_ADOPTION).unwrap();
        for unit in ["dep.service", "r.service", "a.service", "pub.service"] {
            assert_eq!(f.unit(unit), "active", "{unit}");
        }
    }

    /// A worker that logs each run and leaves what Harbor would: admission
    /// when given the staged receipt, else the hand-off evidence.
    fn counting_worker(f: &Fixture) {
        executable(
            &f.p("systemd-run"),
            &format!(
                r#"echo "systemd-run $*" >> {log}
case "$*" in *LoadCredential*) touch {admitted}; exit 0 ;; esac
echo '{{"backup_id":"backup-1"}}' > {evidence}/preparation.json
echo '{{"backup_id":"backup-1","writer_fence_token":"{TOKEN}"}}' > {evidence}/records.json
mkdir -p {export}/backup-1; printf '{{}}' > {export}/backup-1/recovery-export.json
exit 1"#,
                log = f.p("calls.log").display(),
                admitted = f.p("evidence/.admitted").display(),
                evidence = f.p("evidence").display(),
                export = f.p("export").display(),
            ),
        );
    }

    #[test]
    fn a_hand_off_finished_before_it_was_journaled_is_recovered_not_rerun() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        counting_worker(&f);
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "prepared");
        // Killed after the worker exited, before its outcome was saved.
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Fenced;
        journal.export = None;
        journal.export_sha256 = None;
        rollout.save(&journal).unwrap();
        let status = rollout.run(Action::Prepare).unwrap();
        assert_eq!(status["phase"], "prepared");
        assert_eq!(status["export_sha256"], codec::digest(b"{}"));
        // The worker (and its export command) ran once.
        assert_eq!(f.calls().matches("systemd-run ").count(), 1);
    }

    #[test]
    fn an_admission_finished_before_it_was_journaled_is_recovered_not_rerun() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        counting_worker(&f);
        let bytes = br#"{"status":"ready"}"#;
        staged(&f, &rollout, "import/receipt.json", bytes);
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "imported");
        // Killed after the worker admitted, before its outcome was saved: both
        // a retried `prepare` and a retried `import` recover it.
        let killed = || {
            let mut journal = rollout.journal().unwrap().unwrap();
            journal.phase = Phase::Prepared;
            rollout.save(&journal).unwrap();
        };
        killed();
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "imported");
        killed();
        let receipt = f.p("receipt.json");
        fs::write(&receipt, bytes).unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let status = rollout
            .run(Action::Import {
                receipt,
                sha256: codec::digest(bytes),
            })
            .unwrap();
        assert_eq!(status["phase"], "imported");
        assert_eq!(f.calls().matches("systemd-run ").count(), 1);
    }

    #[test]
    fn a_completed_preparation_is_journaled_before_receivers_restart() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| m["receiver_units"] = json!(["r.service"])).unwrap();
        fs::write(f.p("units/r.service"), "active").unwrap();
        rollout.run(FENCE).unwrap();
        // The worker admits the receipt, then restoring the receiver fails.
        executable(
            &f.p("systemd-run"),
            &format!(
                "echo \"systemd-run $*\" >> {}\nexit 0",
                f.p("calls.log").display()
            ),
        );
        let systemctl = fail_start(&f, "r.service");
        assert!(rollout.run(Action::Prepare).is_err());
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(
            journal.phase,
            Phase::Imported,
            "the outcome is journaled first"
        );
        assert_eq!(journal.pending_receivers, ["r.service"]);
        // The retry, now adopting, resumes the receiver and never reruns
        // preparation (and its export command).
        fs::write(f.p("systemctl"), systemctl).unwrap();
        assert_eq!(rollout.run(NEXT).unwrap()["phase"], "adopted");
        assert_eq!(f.unit("r.service"), "active");
        assert_eq!(f.calls().matches("systemd-run ").count(), 1);
    }

    #[test]
    fn templates_and_conflicting_provisions_are_refused_at_load() {
        let f = Fixture::new(0);
        for (field, value) in [
            ("paused_publishers", json!(["pub@.service"])),
            ("receiver_units", json!(["r@.service"])),
            ("setup_units", json!(["setup@.service"])),
        ] {
            let error = edit_manifest(&f, |m| m[field] = value.clone())
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(error.contains("not templates"), "{field}: {error}");
            f.manifest("rollout.json", 2);
        }
        // An export root also listed as a generic provision directory.
        let export = f.p("export");
        let error = edit_manifest(&f, |m| {
            m["prepare"]["provision_directories"] = json!([
                {"path": export, "group": "postgres", "mode": 0o700},
                {"path": export, "group": "users", "mode": 0o750},
            ])
        })
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
        assert!(error.contains("provisioned twice"), "{error}");
        f.manifest("rollout.json", 2);
        assert!(
            edit_manifest(&f, |m| {
                m["prepare"]["provision_directories"] = json!([
                    {"path": export, "group": "users", "mode": 0o750},
                    {"path": export, "group": "users", "mode": 0o750},
                ])
            })
            .is_ok()
        );
    }

    #[test]
    fn a_failed_setup_unit_fails_the_fence_and_can_be_retried() {
        let f = Fixture::new(0);
        fs::write(f.p("units/setup.service"), "failed").unwrap();
        let error = f.rollout().run(FENCE).unwrap_err().to_string();
        assert!(error.contains("setup unit setup.service failed"), "{error}");
        assert!(!f.calls().contains("harbor inspect-fence"));
        let journal = f.rollout().journal().unwrap().unwrap();
        assert_eq!(journal.phase, Phase::Idle);
        // Once fixed (a oneshot ends inactive), the retry fences.
        fs::write(f.p("units/setup.service"), "inactive").unwrap();
        assert_eq!(f.rollout().run(FENCE).unwrap()["phase"], "fenced");
    }

    #[test]
    fn the_execution_lock_must_sit_in_a_directory_only_root_can_modify() {
        let f = Fixture::new(0);
        let locked = |path: PathBuf| {
            let rollout = edit_manifest(&f, |m| m["execution_lock"] = json!(path)).unwrap();
            let result = rollout.run(FENCE).map(drop);
            f.manifest("rollout.json", 2);
            result
        };
        fs::create_dir_all(f.p("locks")).unwrap();
        fs::write(f.p("locks/switch.lock"), "").unwrap();
        // Reached through a symlinked directory, its inode could be swapped.
        std::os::unix::fs::symlink(f.p("locks"), f.p("linked")).unwrap();
        let error = locked(f.p("linked/switch.lock")).unwrap_err().to_string();
        assert!(error.contains("untrusted ancestry"), "{error}");
        // ... or in a directory others can write.
        fs::create_dir(f.p("open")).unwrap();
        fs::set_permissions(f.p("open"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::write(f.p("open/switch.lock"), "").unwrap();
        let error = locked(f.p("open/switch.lock")).unwrap_err().to_string();
        assert!(error.contains("only root can modify"), "{error}");
        // The lock itself is never followed.
        std::os::unix::fs::symlink(f.p("locks/switch.lock"), f.p("locks/link.lock")).unwrap();
        assert!(locked(f.p("locks/link.lock")).is_err());
        assert!(!f.p("calls.log").exists() || !f.calls().contains("inhibit-startup"));
        locked(f.p("locks/switch.lock")).unwrap();
    }

    #[test]
    fn another_manifest_is_refused_and_named() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let other = f.load(&f.manifest("other.json", 5)).unwrap();
        let opener = format!("--config {}", f.p("rollout.json").display());
        let before = f.calls();
        let mut journal = rollout.journal().unwrap().unwrap();
        for action in [NO_ADOPTION, FENCE, NEXT, Action::Prepare] {
            let error = other.run(action).unwrap_err().to_string();
            assert!(error.contains(&opener), "{error}");
        }
        // Fully unwound, an abort is still the opener's to step on, but
        // archiving it (with no receipt left to retire) is anyone's.
        journal.phase = Phase::Aborted;
        journal.reopened = true;
        rollout.save(&journal).unwrap();
        fs::remove_file(f.p("authority/writer-fence.json")).unwrap();
        for action in [NO_ADOPTION, Action::Prepare] {
            let error = other.run(action).unwrap_err().to_string();
            assert!(error.contains(&opener), "{error}");
        }
        assert_eq!(
            f.calls()[before.len()..].matches("systemctl stop").count(),
            0
        );
        assert_eq!(other.run(FENCE).unwrap()["phase"], "fenced");
        assert!(rollout.archives().unwrap()[0].ends_with(".aborted.json"));
        // A thawed rollout leaves nothing open: any manifest archives it too.
        let mut journal = other.journal().unwrap().unwrap();
        journal.phase = Phase::Thawed;
        other.save(&journal).unwrap();
        fs::remove_file(f.p("authority/writer-fence.json")).unwrap();
        assert_eq!(f.rollout().run(FENCE).unwrap()["phase"], "fenced");
    }

    #[test]
    fn the_opening_manifest_is_rooted_while_its_journal_is_open() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        let root = f.p("gcroots").join(GC_ROOT);
        rollout.run(FENCE).unwrap();
        assert_eq!(fs::read_link(&root).unwrap(), f.p("rollout.json"));
        rollout.run(NO_ADOPTION).unwrap();
        assert!(fs::symlink_metadata(&root).is_ok());
        rollout.archive_finished().unwrap();
        assert!(fs::symlink_metadata(&root).is_err());
        // No stray staging link is left in the roots directory.
        assert_eq!(fs::read_dir(f.p("gcroots")).unwrap().count(), 0);
    }

    #[test]
    fn rollout_units_each_have_one_role() {
        let f = Fixture::new(0);
        for (field, value) in [
            ("retired_clients", json!(["pub.service"])),
            ("receiver_units", json!(["a.service"])),
            ("clients", json!(["postgresql.service"])),
            ("receiver_units", json!(["setup.service"])),
            ("clients", json!(["a@.service"])),
        ] {
            let edited = edit_manifest(&f, |m| {
                m[field] = value.clone();
                if value == json!(["a@.service"]) {
                    m["paused_publishers"] = json!(["a@backup.service"]);
                }
            });
            let error = edited
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(error.contains("one role"), "{field}={value}: {error}");
            f.manifest("rollout.json", 2);
        }
        // A client may be retired too (stopped, never restarted).
        assert!(edit_manifest(&f, |m| m["retired_clients"] = json!(["a.service"])).is_ok());
    }

    #[test]
    fn a_static_preparation_failure_is_refused_before_the_fence() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("export"), "group": "users", "mode": 0o750}])
        })
        .unwrap();
        // An existing export root (perhaps a mountpoint) the worker cannot
        // write is never re-owned: fencing refuses before any side effect.
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(error.contains("never re-owned"), "{error}");
        let read_only = |f: &Fixture| {
            let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
            assert!(
                calls
                    .lines()
                    .all(|line| line.starts_with("systemctl show ")),
                "{calls}"
            );
        };
        read_only(&f);
        assert!(rollout.journal().unwrap().is_none());
        assert_eq!(fs::metadata(f.p("export")).unwrap().mode() & 0o7777, 0o755);
        // An absent one must be creatable: not below a directory others write
        // (even a sticky one, where its name could be taken first).
        fs::create_dir(f.p("shared")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o1777)).unwrap();
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("shared/export")]);
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("shared/export"), "group": "users", "mode": 0o750}])
        })
        .unwrap();
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(error.contains("untrusted ancestry"), "{error}");
        read_only(&f);
        // Creatable, it fences.
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
    }

    #[test]
    fn archives_finished_in_the_same_second_never_overwrite_each_other() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        for receipt in [br#"{"n":1}"#, br#"{"n":2}"#] {
            rollout.run(FENCE).unwrap();
            staged(&f, &rollout, "import/receipt.json", receipt);
            rollout.run(NO_ADOPTION).unwrap();
            rollout.archive_finished().unwrap();
        }
        let archives = rollout.archives().unwrap();
        assert_eq!(
            archives,
            [
                "rollout.1.aborted.json",
                "rollout.1.receipt.json",
                "rollout.2.aborted.json",
                "rollout.2.receipt.json"
            ],
        );
        for (id, receipt) in [(1, br#"{"n":1}"#), (2, br#"{"n":2}"#)] {
            let archived = f.p("state").join(format!("rollout.{id}.receipt.json"));
            assert_eq!(fs::read(archived).unwrap(), receipt);
        }
    }

    #[test]
    fn an_inhibition_left_without_its_token_is_recovered_by_abort() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        // Killed after `inhibit-startup` succeeded, before its token was saved:
        // only the intent is journaled, and Harbor holds the inhibition.
        let mut journal = rollout.bound().unwrap();
        journal.inhibiting = true;
        rollout.save(&journal).unwrap();
        fs::write(
            f.p("startup/inhibited.json"),
            json!({ "token": TOKEN }).to_string(),
        )
        .unwrap();
        assert!(guard_at(&f.p("state")).is_err(), "activation must wait");
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert!(!f.p("startup/inhibited.json").exists());
        assert_eq!(f.unit("postgresql.service"), "active");
        // `fence` saves the intent before calling Harbor.
        let f = Fixture::new(0);
        let harbor = fs::read_to_string(f.p("harbor")).unwrap();
        fs::write(
            f.p("harbor"),
            harbor.replacen(
                "case \"$1\" in",
                "[ \"$1\" = inhibit-startup ] && exit 1\ncase \"$1\" in",
                1,
            ),
        )
        .unwrap();
        assert!(f.rollout().run(FENCE).is_err());
        assert!(f.rollout().journal().unwrap().unwrap().inhibiting);
    }

    #[test]
    fn a_journal_less_inhibition_is_reported_and_adopted_explicitly() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        fs::write(
            f.p("startup/inhibited.json"),
            json!({ "token": TOKEN }).to_string(),
        )
        .unwrap();
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(
            error.contains(&format!("--adopt-startup-token {TOKEN}")),
            "{error}"
        );
        let adopting = |token: &str| Action::Abort {
            adopt_fence_token: None,
            adopt_startup_token: Some(token.to_owned()),
        };
        let error = rollout
            .run(adopting(&"f".repeat(32)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("differs"), "{error}");
        assert!(!f.p("calls.log").exists() || !f.calls().contains("harbor fence-open"));
        let status = rollout.run(adopting(TOKEN)).unwrap();
        assert_eq!(status["phase"], "aborted");
        assert!(status["next"].as_str().unwrap().contains("writer clients"));
        // Harbor releases startup at a fence boundary: completed, then closed.
        let calls = f.calls();
        assert!(calls.contains("harbor fence-open") && calls.contains("harbor fence-close"));
        assert!(!f.p("startup/inhibited.json").exists());
        assert_eq!(f.unit("postgresql.service"), "active");
        // Never alongside a journal.
        let error = rollout.run(adopting(TOKEN)).unwrap_err().to_string();
        assert!(error.contains("journal exists"), "{error}");
    }

    #[test]
    fn a_failed_ownership_change_never_publishes_a_provisioned_directory() {
        if root() {
            return;
        }
        let f = Fixture::new(0);
        fs::remove_dir(f.p("export")).unwrap();
        let mut rollout = edit_manifest(&f, |m| {
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("export"), "group": "postgres", "mode": 0o750}])
        })
        .unwrap();
        // A non-root test cannot hand a directory to root: the change fails
        // as a crash before it would, and nothing may look provisioned.
        rollout.service_owner = Some((0, 0));
        assert!(rollout.provision_all().is_err());
        assert!(!f.p("export").exists());
        rollout.service_owner = Some((euid(), unsafe { libc::getegid() }));
        rollout.provision_all().unwrap();
        let metadata = fs::metadata(f.p("export")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o750);
    }

    #[test]
    fn files_other_writers_control_are_read_through_one_pinned_descriptor() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        let fifo = |path: &Path| {
            let _ = fs::remove_file(path);
            let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        };
        // A FIFO in Harbor's fence selector would hang the refusal check.
        fifo(&f.p("data/postgresql.auto.conf"));
        assert!(rollout.run(FENCE).is_err());
        fs::remove_file(f.p("data/postgresql.auto.conf")).unwrap();
        rollout.run(FENCE).unwrap();
        // ... in the preparation evidence, the hand-off read.
        fifo(&f.p("evidence/preparation.json"));
        assert!(rollout.run(Action::Prepare).is_err());
        // ... and in place of a transported receipt or the acceptance.
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Prepared;
        rollout.save(&journal).unwrap();
        let receipt = f.p("receipt.json");
        fifo(&receipt);
        let import = |receipt: &Path| {
            rollout
                .run(Action::Import {
                    receipt: receipt.to_owned(),
                    sha256: codec::digest(b"{}"),
                })
                .unwrap_err()
                .to_string()
        };
        assert!(import(&receipt).contains("private mode-0600"));
        // A symlink to a private file is refused too, not followed.
        fs::remove_file(&receipt).unwrap();
        fs::write(f.p("real.json"), "{}").unwrap();
        fs::set_permissions(f.p("real.json"), fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(f.p("real.json"), &receipt).unwrap();
        assert!(import(&receipt).contains("private mode-0600"));
        journal.phase = Phase::Activated;
        rollout.save(&journal).unwrap();
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let acceptance = f.p("acceptance.json");
        fifo(&acceptance);
        let error = rollout
            .run(Action::Thaw { acceptance })
            .unwrap_err()
            .to_string();
        assert!(error.contains("root-owned"), "{error}");
    }

    #[test]
    fn a_newer_coordinators_journal_still_loads_and_aborts() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let path = f.p("state/rollout.json");
        let mut journal = durable::read_json(&path).unwrap();
        // Empty and absent values are not written.
        for key in ["export", "receipt_sha256", "pending_receivers", "reopened"] {
            assert!(journal.get(key).is_none(), "{key}: {journal}");
        }
        // A later generation's additive fields, unknown to this one.
        journal["transported"] = json!({"receipt": "/srv/x", "sha256": "0".repeat(64)});
        journal["future"] = json!({"written": ["/run/x"]});
        durable::write_json(&path, &journal).unwrap();
        assert_eq!(rollout.status().unwrap()["phase"], "fenced");
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("postgresql.service"), "active");
    }

    #[test]
    fn a_finished_rollouts_staged_receipt_is_archived_not_left_in_the_slot() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // `import` staged and recorded a receipt, then the rollout was aborted.
        let bytes = br#"{"status":"ready"}"#;
        staged(&f, &rollout, "import/receipt.json", bytes);
        rollout.run(NO_ADOPTION).unwrap();
        // The next rollout starts fresh and prepares past the import slot.
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
        assert!(!f.p("import/receipt.json").exists());
        let receipts: Vec<_> = (rollout.archives().unwrap().into_iter())
            .filter(|name| name.ends_with(".receipt.json"))
            .collect();
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        let archived = f.p("state").join(&receipts[0]);
        assert_eq!(fs::read(&archived).unwrap(), bytes);
        assert_eq!(fs::metadata(&archived).unwrap().mode() & 0o777, 0o600);
        reported(&f, "backup-1");
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "prepared");
    }

    #[test]
    fn declared_parents_are_provisioned_before_nested_children() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("backup")]);
            m["prepare"]["provision_directories"] = json!([
                {"path": f.p("backup/evidence"), "group": "postgres", "mode": 0o700},
                {"path": f.p("backup"), "group": "users", "mode": 0o750},
            ])
        })
        .unwrap();
        rollout.provision_all().unwrap();
        for (path, mode) in [("backup", 0o750), ("backup/evidence", 0o700)] {
            let metadata = fs::symlink_metadata(f.p(path)).unwrap();
            assert_eq!(metadata.mode() & 0o7777, mode, "{path}");
            assert_eq!(metadata.uid(), euid(), "{path}");
        }
    }

    #[test]
    fn mutating_steps_hold_the_shared_execution_lock_but_status_does_not() {
        let f = Fixture::new(0);
        let lock = f.p("switch.lock");
        fs::write(&lock, "").unwrap();
        let rollout = edit_manifest(&f, |m| m["execution_lock"] = json!(lock)).unwrap();
        let held = durable::lock(&lock, false, false).unwrap();
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(
            error.contains("execution lock") && error.contains(&std::process::id().to_string()),
            "{error}"
        );
        assert!(!f.p("calls.log").exists(), "no step ran without the lock");
        assert_eq!(rollout.status().unwrap()["phase"], "idle");
        drop(held);
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
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
        rollout.run(FENCE).unwrap();
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
        let before = f.calls();
        // Every mutating step, unwinding included, is refused before any side
        // effect: no fence closes and no writer starts under the worker.
        for error in [
            rollout.run(Action::Prepare).unwrap_err(),
            rollout.run(NO_ADOPTION).unwrap_err(),
            rollout.run(FENCE).unwrap_err(),
            rollout.run(NEXT).unwrap_err(),
            rollout.run(Action::Adopt).unwrap_err(),
            rollout
                .run(Action::Thaw {
                    acceptance: f.p("acceptance.json"),
                })
                .unwrap_err(),
            rollout.lift_pause(&mut journal).unwrap_err(),
            rollout.reopen(&mut journal, None).unwrap_err(),
        ] {
            assert!(error.to_string().contains(PREPARE_UNIT), "{error}");
        }
        let calls = f.calls();
        let after = &calls[before.len()..];
        assert!(
            after
                .lines()
                .all(|line| line.starts_with("systemctl show ")),
            "{after}"
        );
        assert_eq!(f.unit("a.service"), "inactive");
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
        let error = worker_ready(&rollout, &mut journal);
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
        rollout.run(FENCE).unwrap();
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
        rollout.run(FENCE).unwrap();
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
        rollout.run(FENCE).unwrap();
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
                adopt_startup_token: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("differs"), "{error}");
        assert!(!f.calls().contains("fence-close"));
        let status = rollout
            .run(Action::Abort {
                adopt_fence_token: Some(TOKEN.into()),
                adopt_startup_token: None,
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
            let error = worker_ready(&rollout, &mut rollout.bound().unwrap())
                .unwrap_err()
                .to_string();
            f.manifest("rollout.json", 2);
            error
        };
        // A symlinked export root would make systemd grant the target write access.
        fs::remove_dir(f.p("export")).unwrap();
        std::os::unix::fs::symlink(f.p("data"), f.p("export")).unwrap();
        let error = prepare(&|_| {});
        assert!(error.contains("untrusted ancestry"), "{error}");
        fs::remove_file(f.p("export")).unwrap();
        fs::create_dir(f.p("export")).unwrap();
        // So would a writable path whose parent another user can modify.
        fs::create_dir_all(f.p("shared/backups")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o777)).unwrap();
        let error = prepare(&|m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("shared/backups")])
        });
        assert!(error.contains("untrusted ancestry"), "{error}");
        // Provisioning never follows (nor re-permissions) a symlink.
        std::os::unix::fs::symlink(f.p("data"), f.p("provisioned")).unwrap();
        let mode = fs::metadata(f.p("data")).unwrap().mode();
        let error = prepare(&|m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("provisioned")]);
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
        rollout.run(FENCE).unwrap();
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
        rollout.run(FENCE).unwrap();
        let aborted = rollout.run(NO_ADOPTION).unwrap();
        assert!(
            aborted["next"]
                .as_str()
                .unwrap()
                .contains("rollout archive"),
            "{aborted}"
        );
        // An abort whose fence is held again is not fully unwound.
        let marker = f.p("authority/writer-fence.json");
        fs::write(&marker, json!({ "token": "f".repeat(32) }).to_string()).unwrap();
        let error = rollout.run(FENCE).unwrap_err().to_string();
        assert!(error.contains("not fully unwound"), "{error}");
        assert!(rollout.archives().unwrap().is_empty());
        fs::remove_file(&marker).unwrap();
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
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
        rollout.run(FENCE).unwrap();
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
                json!(["/srv/backups", f.p("evidence")]))
            .is_ok()
        );
    }

    #[test]
    fn preparation_never_provisions_below_an_absent_mount() {
        let f = Fixture::new(0);
        fs::create_dir(f.p("mnt")).unwrap();
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["required_mounts"] = json!([f.p("mnt")]);
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("mnt/export")]);
            m["prepare"]["provision_directories"] =
                json!([{"path": f.p("mnt/export"), "group": "postgres", "mode": 488}]);
        })
        .unwrap();
        let error = worker_ready(&rollout, &mut rollout.bound().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("required mount"), "{error}");
        assert!(!f.p("mnt/export").exists());
        assert!(!f.calls().contains("systemd-run"));
    }

    #[test]
    fn every_static_contract_of_a_post_fence_step_is_refused_before_the_fence() {
        type Setup = fn(&Fixture) -> Value;
        let mode = |f: &Fixture, path: &str, mode: u32| {
            fs::set_permissions(f.p(path), fs::Permissions::from_mode(mode)).unwrap();
        };
        let cases: [(&str, Setup, &str); 14] = [
            // A writable path the worker cannot write (as a root 0755 one is).
            (
                "unwritable",
                |f| {
                    fs::create_dir(f.p("ro")).unwrap();
                    fs::set_permissions(f.p("ro"), fs::Permissions::from_mode(0o555)).unwrap();
                    json!({"read_write_paths": [f.p("evidence"), f.p("ro")]})
                },
                "not writable by postgres",
            ),
            // A recovery directory reached through a symlink below its
            // writable root.
            (
                "redirected",
                |f| {
                    fs::create_dir_all(f.p("elsewhere/repository")).unwrap();
                    fs::create_dir(f.p("vault")).unwrap();
                    std::os::unix::fs::symlink(f.p("elsewhere"), f.p("vault/link")).unwrap();
                    let path = f.p("maintenance.json");
                    let mut maintenance = durable::read_json(&path).unwrap();
                    maintenance["recovery"]["backup_root"] = json!(f.p("vault/link/repository"));
                    fs::write(&path, maintenance.to_string()).unwrap();
                    json!({"read_write_paths": [f.p("evidence"), f.p("vault")]})
                },
                "untrusted ancestry",
            ),
            // A receipt left by another writer window, without its snapshot.
            (
                "receipt",
                |f| {
                    let path = f.p("maintenance.json");
                    let mut maintenance = durable::read_json(&path).unwrap();
                    maintenance["recovery"]["receipt_file"] = json!(f.p("evidence/recovery.json"));
                    fs::write(&path, maintenance.to_string()).unwrap();
                    fs::write(f.p("evidence/recovery.json"), "{}").unwrap();
                    json!({})
                },
                "evidence/recovery.json",
            ),
            // The backup root the worker's backup command fills.
            (
                "backup root",
                |f| {
                    let path = f.p("maintenance.json");
                    let mut maintenance = durable::read_json(&path).unwrap();
                    maintenance["recovery"]["backup_root"] = json!(f.p("vault/repository"));
                    fs::write(&path, maintenance.to_string()).unwrap();
                    fs::create_dir(f.p("vault")).unwrap();
                    json!({"read_write_paths": [f.p("evidence"), f.p("vault")]})
                },
                "neither exists nor is provisioned",
            ),
            // The opening manifest's GC root is published there by pathname.
            (
                "gc roots",
                |f| {
                    fs::set_permissions(f.p("gcroots"), fs::Permissions::from_mode(0o777)).unwrap();
                    json!({})
                },
                "only root can write",
            ),
            // Harbor's own mounts (`fence-open` checks them), declared only in
            // the maintenance manifest.
            (
                "maintenance mount",
                |f| {
                    let path = f.p("maintenance.json");
                    let mut maintenance = durable::read_json(&path).unwrap();
                    maintenance["required_mounts"] = json!([f.p("data")]);
                    fs::write(&path, maintenance.to_string()).unwrap();
                    json!({})
                },
                "required mount is absent",
            ),
            // Adoption and the fence write the identity state as postgres.
            (
                "authority",
                |f| {
                    fs::set_permissions(f.p("authority"), fs::Permissions::from_mode(0o755))
                        .unwrap();
                    json!({})
                },
                "never re-owned",
            ),
            // Harbor's inhibition and the publisher pause write as root.
            (
                "startup",
                |f| {
                    fs::set_permissions(f.p("startup"), fs::Permissions::from_mode(0o775)).unwrap();
                    json!({})
                },
                "only root can write",
            ),
            (
                "drop-in root",
                |f| {
                    fs::set_permissions(f.p("control"), fs::Permissions::from_mode(0o777)).unwrap();
                    json!({})
                },
                "only root can write",
            ),
            // systemd fails the worker on a missing writable path.
            (
                "writable path",
                |f| json!({"read_write_paths": [f.p("evidence"), f.p("absent")]}),
                "neither exists nor is provisioned",
            ),
            // Harbor's preparation creates no evidence directory, even one
            // inside a writable path.
            (
                "evidence",
                |f| {
                    let path = f.p("maintenance.json");
                    let mut maintenance = durable::read_json(&path).unwrap();
                    maintenance["recovery"]["receipt_file"] =
                        json!(f.p("evidence/receipts/recovery.json"));
                    fs::write(&path, maintenance.to_string()).unwrap();
                    json!({})
                },
                "neither exists nor is provisioned",
            ),
            // `prepare` refuses a receipt this rollout never recorded.
            (
                "slot",
                |f| {
                    staged_file(&f.p("import/receipt.json"));
                    json!({})
                },
                "did not stage",
            ),
            // `import` would create the slot directory below the absent mount.
            (
                "mount",
                |f| json!({"required_mounts": [f.p("import")]}),
                "required mount",
            ),
            // The worker could not search its way to a writable path.
            (
                "search",
                |f| {
                    fs::create_dir_all(f.p("outer/rw")).unwrap();
                    fs::set_permissions(f.p("outer"), fs::Permissions::from_mode(0o600)).unwrap();
                    json!({"read_write_paths": [f.p("evidence"), f.p("outer/rw")]})
                },
                "not searchable",
            ),
        ];
        for (name, setup, expected) in cases {
            let f = Fixture::new(0);
            let prepare = setup(&f);
            let rollout = edit_manifest(&f, |m| {
                for (key, value) in prepare.as_object().unwrap() {
                    m["prepare"][key] = value.clone();
                }
            })
            .unwrap();
            let error = rollout.run(FENCE).unwrap_err().to_string();
            assert!(error.contains(expected), "{name}: {error}");
            let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
            assert!(
                calls
                    .lines()
                    .all(|line| line.starts_with("systemctl show ")),
                "{name}: {calls}"
            );
            assert!(rollout.journal().unwrap().is_none(), "{name}");
            if f.p("outer").exists() {
                mode(&f, "outer", 0o755);
            }
        }
    }

    fn staged_file(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path, "{}").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn rollout_roots_are_pairwise_disjoint_but_for_intended_nestings() {
        let f = Fixture::new(0);
        let load = |edit: Value| {
            let result = edit_manifest(&f, |m| {
                for (key, value) in edit.as_object().unwrap() {
                    m["prepare"][key] = value.clone();
                }
            });
            f.manifest("rollout.json", 2);
            result.err().map(|error| error.to_string())
        };
        let provision = |path: &str| json!({"path": f.p(path), "group": "postgres", "mode": 448});
        for (edit, expected) in [
            // Harbor's inhibition would make the export root unsearchable.
            (
                json!({"export_root": f.p("startup/export")}),
                "startup inhibition state directory",
            ),
            (
                json!({"read_write_paths": [f.p("evidence"), f.p("data")]}),
                "PostgreSQL data directory",
            ),
            (
                json!({"read_write_paths": [f.p("evidence"), f.p("import")]}),
                "import slot directory",
            ),
            (
                json!({"read_write_paths": [f.p("evidence"), f.p("export/x")],
                    "provision_directories": [provision("export/x")]}),
                "export root",
            ),
            (
                json!({"read_write_paths": [f.p("evidence"), f.p("evidence/x")]}),
                "writable path",
            ),
            // The worker writes evidence and provisioned directories.
            (
                json!({"read_write_paths": []}),
                "outside its writable paths",
            ),
            (
                json!({"provision_directories": [provision("elsewhere")]}),
                "outside its writable paths",
            ),
        ] {
            let error = load(edit.clone()).unwrap_or_default();
            assert!(error.contains(expected), "{edit}: {error}");
        }
        // Evidence in its provisioned backup root, the export root in a
        // writable path, and the export root's own provision contract.
        for edit in [
            json!({"read_write_paths": [f.p("evidence"), f.p("backup")],
                "provision_directories": [provision("backup"), provision("backup/evidence")]}),
            json!({"read_write_paths": [f.p("evidence"), f.p("exports")],
                "export_root": f.p("exports/x")}),
            json!({"provision_directories": [{"path": f.p("export"), "group": "users", "mode": 488}]}),
        ] {
            assert_eq!(load(edit.clone()), None, "{edit}");
        }
        // The backup root the worker's backup command fills is writable too.
        let path = f.p("maintenance.json");
        let mut maintenance = durable::read_json(&path).unwrap();
        maintenance["recovery"]["backup_root"] = json!(f.p("backups"));
        fs::write(&path, maintenance.to_string()).unwrap();
        let error = load(json!({})).unwrap_or_default();
        assert!(
            error.contains("backups is written by the preparation worker"),
            "{error}"
        );
        assert_eq!(
            load(json!({"read_write_paths": [f.p("evidence"), f.p("backups")]})),
            None
        );
    }

    #[test]
    fn local_executables_and_inputs_are_checked_at_load() {
        let f = Fixture::new(0);
        let error = |edit: &dyn Fn(&mut Value)| {
            let result = edit_manifest(&f, |m| edit(m));
            f.manifest("rollout.json", 2);
            result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        };
        let (tool, data) = (store_tool(), store_data_file());
        let directory = tool.parent().unwrap().to_owned();
        // Outside the store, not a regular file, not executable.
        for (field, value) in [
            ("harbor", json!(f.p("harbor"))),
            ("systemctl", json!(directory)),
            ("systemd_run", json!(data)),
        ] {
            let error = error(&|m| m[field] = value.clone());
            assert!(
                error.contains("executable regular store"),
                "{field}: {error}"
            );
        }
        let error_text = error(&|m| m["preparation"] = json!(f.p("preparation.json")));
        assert!(error_text.contains("preparation config"), "{error_text}");
        // Thaw's checker must be runnable, not only a store path.
        let checker = |checker: &Path| {
            error(&|m| m["cutover"] = json!({"checker": checker, "contract": data}))
        };
        assert!(checker(&data).contains("executable regular store"));
        assert_eq!(checker(&tool), "");
        // The executor command runs on another host: checked lexically.
        for (host, user, command) in [
            ("", "can", json!(["/bin/true"])),
            ("executor", "", json!(["/bin/true"])),
            ("executor", "can", json!(["true"])),
            ("executor", "can", json!(["/bin/true", ""])),
            ("executor", "can", json!([])),
            ("primary", "can", json!(["/bin/true"])),
            ("executor", "root", json!(["/bin/true"])),
        ] {
            let error =
                error(&|m| m["executor"] = json!({"host": host, "user": user, "command": command}));
            assert!(error.contains("rollout executor"), "{command}: {error}");
        }
    }

    #[test]
    fn every_name_and_endpoint_a_post_fence_step_uses_is_resolved_before_the_fence() {
        let refused = |edit: &dyn Fn(&Fixture, &mut Value), expected: &str| {
            let f = Fixture::new(0);
            let rollout = edit_manifest(&f, |m| edit(&f, m)).unwrap();
            let error = rollout.run(FENCE).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            let calls = fs::read_to_string(f.p("calls.log")).unwrap_or_default();
            assert!(
                !calls.contains("inhibit-startup") && !calls.contains("systemctl stop"),
                "{calls}"
            );
            assert!(rollout.journal().unwrap().is_none());
        };
        // The group of a directory still to be provisioned.
        refused(
            &|f, m| {
                m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("backup")]);
                m["prepare"]["provision_directories"] =
                    json!([{"path": f.p("backup"), "group": "harbor-db-absent", "mode": 448}]);
            },
            "group harbor-db-absent is absent",
        );
        // A supplementary group of the worker.
        refused(
            &|_, m| m["prepare"]["supplementary_groups"] = json!(["harbor-db-absent"]),
            "group harbor-db-absent is absent",
        );
        // The socket and port `inspect-fence` uses once the fence is open.
        refused(
            &|f, _| {
                let harbor = fs::read_to_string(f.p("harbor")).unwrap();
                let live = "inspect-live) echo '{\"system_identifier\":\"42\"}' ;;";
                assert!(harbor.contains(live));
                fs::write(
                    f.p("harbor"),
                    harbor.replace(
                        live,
                        "inspect-live) echo no server on that socket >&2; exit 1 ;;",
                    ),
                )
                .unwrap();
            },
            "no server on that socket",
        );
    }

    #[test]
    fn provisioning_stays_in_its_pinned_parent_when_the_parent_is_swapped() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("backup")]);
            m["prepare"]["provision_directories"] = json!([
                {"path": f.p("backup"), "group": "postgres", "mode": 0o700},
                {"path": f.p("backup/nested"), "group": "postgres", "mode": 0o700},
            ]);
        })
        .unwrap();
        let backup = f.p("backup");
        rollout.provision(&backup, "postgres", 0o700).unwrap();
        // The walk pins the parent; then its owner (postgres on a host)
        // swaps it for a symlink to a directory of its choosing.
        let (parent, _) = (rollout.walk(&backup, &[&backup], Some(0o755)).unwrap()).unwrap();
        fs::create_dir(f.p("outside")).unwrap();
        fs::set_permissions(f.p("outside"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&backup, f.p("backup.moved")).unwrap();
        std::os::unix::fs::symlink(f.p("outside"), &backup).unwrap();
        let owner = (euid(), unsafe { libc::getegid() });
        (rollout.provision_in(&parent, &f.p("backup/nested"), owner, 0o700)).unwrap();
        // Nothing was made, permissioned or owned through the symlink: the
        // directory landed in the pinned (moved) parent.
        let outside = || {
            let metadata = fs::metadata(f.p("outside")).unwrap();
            (
                fs::read_dir(f.p("outside")).unwrap().count(),
                metadata.mode() & 0o7777,
            )
        };
        assert_eq!(outside(), (0, 0o755));
        let nested = fs::metadata(f.p("backup.moved/nested")).unwrap();
        assert_eq!(nested.mode() & 0o7777, 0o700);
        // The step itself fails closed on the swapped path.
        let error = worker_ready(&rollout, &mut rollout.bound().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("untrusted ancestry"), "{error}");
        assert_eq!(outside(), (0, 0o755));
        assert!(!f.calls().contains("systemd-run"));
        // Never provisioned in an undeclared directory others can write.
        fs::create_dir(f.p("shared")).unwrap();
        fs::set_permissions(f.p("shared"), fs::Permissions::from_mode(0o777)).unwrap();
        let error = (rollout.provision(&f.p("shared/x"), "postgres", 0o700))
            .unwrap_err()
            .to_string();
        assert!(error.contains("untrusted ancestry"), "{error}");
        assert_eq!(fs::read_dir(f.p("shared")).unwrap().count(), 0);
    }

    #[test]
    fn an_import_never_admits_over_a_rewritten_certified_export() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        reported(&f, "backup-1");
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "prepared");
        // The import's preparation reruns the exporter, which rewrites the
        // certified plan, and Harbor admits.
        executable(
            &f.p("systemd-run"),
            &format!(
                r#"echo "systemd-run $*" >> {log}
printf rewritten > {export}/backup-1/recovery-export.json
touch {admitted}"#,
                log = f.p("calls.log").display(),
                export = f.p("export").display(),
                admitted = f.p("evidence/.admitted").display(),
            ),
        );
        // As `import` stages it (staging itself needs root).
        staged(&f, &rollout, "import/receipt.json", b"{}");
        let receipt = f.p("receipt.json");
        fs::write(&receipt, "{}").unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let import = || {
            rollout.run(Action::Import {
                receipt: receipt.clone(),
                sha256: codec::digest(b"{}"),
            })
        };
        let rewritten = |error: Result<Value>| {
            let error = error.unwrap_err().to_string();
            assert!(error.contains("rewrote the certified hand-off"), "{error}");
            assert_eq!(rollout.journal().unwrap().unwrap().phase, Phase::Prepared);
        };
        rewritten(import());
        // A retry recovering the admission is held to the same hand-off.
        rewritten(import());
        // With the certified export restored, the admission is recorded.
        fs::write(f.p("export/backup-1/recovery-export.json"), "{}").unwrap();
        assert_eq!(import().unwrap()["phase"], "imported");
    }

    #[test]
    fn publishers_whose_start_the_pause_skipped_are_started_when_it_lifts() {
        let f = Fixture::new(0);
        let units = ["cand", "old", "ran", "never", "gone"].map(|unit| format!("{unit}.service"));
        let rollout = edit_manifest(&f, |m| {
            m["paused_publishers"] = json!(["pub.service"]);
            m["paused_publishers"]
                .as_array_mut()
                .unwrap()
                .extend(units.iter().map(|u| json!(u)));
        })
        .unwrap();
        rollout.run(FENCE).unwrap();
        // The marker keeps when the pause began, even if pausing is retried.
        let marker = f.p("state/paused");
        let began = std::time::SystemTime::now() - Duration::from_secs(60);
        (fs::File::options().write(true).open(&marker).unwrap())
            .set_modified(began)
            .unwrap();
        rollout
            .pause_publishers(&mut rollout.journal().unwrap().unwrap())
            .unwrap();
        let paused = fs::metadata(&marker).unwrap().mtime();
        assert_eq!(fs::metadata(&marker).unwrap().modified().unwrap(), began);
        // The candidate's publisher was loaded and its start skipped by the
        // pause; another's last failed condition predates it; one ran; one
        // never did; one is not loaded at all.
        for (unit, condition, loaded) in [
            ("cand.service", Some(("no", paused)), true),
            ("old.service", Some(("no", paused - 3600)), true),
            ("ran.service", Some(("yes", paused)), true),
            ("never.service", None, true),
            ("gone.service", Some(("no", paused)), false),
        ] {
            if loaded {
                fs::write(f.p("units").join(unit), "inactive").unwrap();
            }
            if let Some((result, at)) = condition {
                let file = f.p("units").join(format!(".condition-{unit}"));
                fs::write(file, format!("{result} {at}")).unwrap();
            }
        }
        rollout.run(NO_ADOPTION).unwrap();
        let calls = f.calls();
        assert!(
            calls.contains("--no-block start -- cand.service"),
            "{calls}"
        );
        assert_eq!(f.unit("cand.service"), "active");
        for unit in [
            "old.service",
            "ran.service",
            "never.service",
            "gone.service",
        ] {
            assert!(
                !calls.contains(&format!("start -- {unit}")),
                "{unit}: {calls}"
            );
        }
        assert_eq!(f.unit("pub.service"), "active");
    }

    #[test]
    fn a_zero_job_id_means_no_queued_job() {
        let f = Fixture::new(0);
        // systemctl renders no job as an empty value; D-Bus's id 0 means the same.
        let systemctl = fs::read_to_string(f.p("systemctl")).unwrap();
        let empty = r#"[ "$prop" = --property=Job ] && echo "" ||"#;
        assert!(systemctl.contains(empty));
        let zero = systemctl.replace(empty, r#"[ "$prop" = --property=Job ] && echo 0 ||"#);
        fs::write(f.p("systemctl"), zero).unwrap();
        let rollout = f.rollout();
        // Settled setup units let the fence verify.
        assert_eq!(rollout.run(FENCE).unwrap()["phase"], "fenced");
        // A queued start whose oneshot already ended counts as started.
        inject(
            &f,
            "systemctl",
            "[ \"$*\" = \"--no-block start -- a.service\" ] && { echo inactive > \"$u/a.service\"; exit 0; }",
        );
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
    }

    #[test]
    fn units_the_running_generation_does_not_load_are_skipped_not_wedging_the_unwind() {
        let f = Fixture::new(0);
        // A publisher only the candidate brings (as a lost-journal abort seeds
        // every declared one) and a recorded client the candidate removed.
        let rollout = edit_manifest(&f, |m| {
            m["paused_publishers"] = json!(["pub.service", "cand.service"])
        })
        .unwrap();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.clients.push("gone.service".into());
        journal.stopped_publishers.push("cand.service".into());
        rollout.save(&journal).unwrap();
        let status = rollout.run(NO_ADOPTION).unwrap();
        assert_eq!(status["phase"], "aborted");
        assert_eq!(status["unloaded"], json!(["gone.service", "cand.service"]));
        let calls = f.calls();
        for unit in ["gone.service", "cand.service"] {
            assert!(
                !calls.contains(&format!("start -- {unit}")),
                "{unit}: {calls}"
            );
        }
        assert_eq!(f.unit("a.service"), "active");
        assert_eq!(f.unit("pub.service"), "active");
    }

    #[test]
    fn a_start_still_queued_behind_another_job_counts_as_started() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        // b waits (After=) on a long oneshot: inactive with its start job queued.
        inject(
            &f,
            "systemctl",
            "[ \"$*\" = \"--no-block start -- b.service\" ] && exit 0\n\
             [ \"$*\" = \"show b.service --value --property=Job\" ] && { echo 7; exit 0; }",
        );
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("b.service"), "inactive");
        assert_eq!(f.unit("a.service"), "active");
    }

    #[test]
    fn a_rolled_back_activation_can_be_aborted() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Activated;
        rollout.save(&journal).unwrap();
        // Installed, the candidate would run unfenced without an acceptance.
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(
            error.contains("roll back to the previous generation"),
            "{error}"
        );
        // Rolled back (failed verification, or stale evidence), it unwinds.
        fs::remove_file(f.p("installed.json")).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(f.unit("a.service"), "active");
        // A thawed rollout is final.
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Thawed;
        rollout.save(&journal).unwrap();
        let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
        assert!(error.contains("thawed"), "{error}");
    }

    #[test]
    fn accept_writes_the_acceptance_thaw_checks() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        fs::write(
            f.p("evidence/records.json"),
            r#"{"writer_fence_token":"x"}"#,
        )
        .unwrap();
        // Only an activated rollout whose candidate is installed is accepted.
        assert!(rollout.run(Action::Accept).is_err());
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Activated;
        rollout.save(&journal).unwrap();
        let error = rollout.run(Action::Accept).unwrap_err().to_string();
        assert!(error.contains("activate the candidate first"), "{error}");
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let status = rollout.run(Action::Accept).unwrap();
        let path = f.p("state/acceptance.json");
        assert!(
            status["next"]
                .as_str()
                .unwrap()
                .contains(&format!("rollout thaw --acceptance {}", path.display())),
            "{status}"
        );
        // Private to root in the root-private journal root, and exactly what
        // thaw checks.
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!((metadata.uid(), metadata.mode() & 0o777), (euid(), 0o600));
        let accepted = durable::read_json(&path).unwrap();
        rollout.check_acceptance(&journal, &accepted).unwrap();
        assert_eq!(accepted["token"], TOKEN);
    }

    #[test]
    fn hints_never_offer_a_step_the_journal_refuses() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        // Unwinding: only the unwind continues.
        journal.reopened = true;
        for phase in [
            Phase::Fenced,
            Phase::Prepared,
            Phase::Imported,
            Phase::Adopted,
        ] {
            journal.phase = phase;
            let hint = rollout.next_hint(&journal);
            assert_eq!(
                hint, "the abort is unwinding; rerun `rollout abort`",
                "{phase:?}"
            );
        }
        // Activated: an unwinding thaw needs the candidate installed, an
        // unwinding abort (after a rollback) needs it gone.
        journal.phase = Phase::Activated;
        let hint = rollout.next_hint(&journal).to_string();
        assert!(hint.contains("rerun `rollout abort`"), "{hint}");
        std::os::unix::fs::symlink(f.p("rollout.json"), f.p("installed.json")).unwrap();
        let hint = rollout.next_hint(&journal).to_string();
        assert!(hint.contains("rerun `rollout thaw`"), "{hint}");
        fs::remove_file(f.p("installed.json")).unwrap();
        // Thawed: retire the rollout, never a fresh one.
        journal.phase = Phase::Thawed;
        let hint = rollout.next_hint(&journal).to_string();
        assert!(
            hint.contains("retire the rollout") && hint.contains("requireWriterFence"),
            "{hint}"
        );
        assert!(!hint.contains("fresh"), "{hint}");
        // Activated: accept and thaw, or roll back and abort.
        journal.reopened = false;
        journal.phase = Phase::Activated;
        let hint = rollout.next_hint(&journal).to_string();
        assert!(
            hint.contains("rollout accept") && hint.contains("roll back"),
            "{hint}"
        );
    }

    #[test]
    fn clients_whose_start_the_fence_skipped_are_started_when_it_reopens() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["clients"] = json!(["a.service", "b.service", "c@.service", "old.service"])
        })
        .unwrap();
        rollout.run(FENCE).unwrap();
        // A client the candidate introduced was skipped by the fence's
        // condition; another's last failed condition predates the fence.
        let fenced = fs::metadata(f.p("authority/writer-fence.json"))
            .unwrap()
            .mtime();
        for (unit, at) in [("c@x.service", fenced), ("old.service", fenced - 3600)] {
            fs::write(f.p("units").join(unit), "inactive").unwrap();
            let file = f.p("units").join(format!(".condition-{unit}"));
            fs::write(file, format!("no {at}")).unwrap();
        }
        rollout.run(NO_ADOPTION).unwrap();
        let calls = f.calls();
        assert!(calls.contains("--no-block start -- c@x.service"), "{calls}");
        assert_eq!(f.unit("c@x.service"), "active");
        assert!(!calls.contains("start -- old.service"), "{calls}");
        assert_eq!(f.unit("old.service"), "inactive");
    }

    #[test]
    fn a_finished_rollout_is_archived_only_explicitly_and_never_refenced_by_next() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        rollout.run(NO_ADOPTION).unwrap();
        let inhibits = || f.calls().matches("harbor inhibit-startup").count();
        let fenced = inhibits();
        // `next` after an abort reports; it neither archives nor fences again.
        let status = (rollout.run(Action::Next {
            adopt_fence_token: None,
        }))
        .unwrap();
        assert_eq!(status["phase"], "aborted");
        assert_eq!(inhibits(), fenced);
        assert!(rollout.archives().unwrap().is_empty());
        // `archive` retires the journal and its GC root without fencing.
        assert_eq!(rollout.run(Action::Archive).unwrap()["phase"], "idle");
        assert_eq!(rollout.archives().unwrap().len(), 1);
        assert!(!f.p("gcroots").join(GC_ROOT).exists());
        assert_eq!(inhibits(), fenced);
        // An unfinished rollout is not archived.
        rollout.run(FENCE).unwrap();
        let error = rollout.run(Action::Archive).unwrap_err().to_string();
        assert!(error.contains("not finished"), "{error}");
    }

    #[test]
    fn a_retried_unwind_never_restarts_a_client_it_already_started() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["clients"] = json!(["a.service", "b.service", "once.service"])
        })
        .unwrap();
        fs::write(f.p("units/once.service"), "active").unwrap();
        rollout.run(FENCE).unwrap();
        // a's start fails; once is a oneshot that runs and ends, after which
        // a preparation worker appears, so every later unwind step refuses
        // before saving: only once's own record survives.
        let systemctl = fail_start(&f, "a.service");
        inject(
            &f,
            "systemctl",
            "[ \"$*\" = \"--no-block start -- once.service\" ] && { echo inactive > \"$u/once.service\"; \
             echo active > \"$u/harbor-db-rollout-prepare.service\"; exit 0; }",
        );
        assert!(rollout.run(NO_ADOPTION).is_err());
        let starts = || {
            f.calls()
                .matches("--no-block start -- once.service")
                .count()
        };
        assert_eq!(starts(), 1);
        // The retry repeats only the failed start.
        fs::write(f.p("systemctl"), systemctl).unwrap();
        fs::remove_file(f.p("units/harbor-db-rollout-prepare.service")).unwrap();
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
        assert_eq!(starts(), 1);
        assert_eq!(f.unit("a.service"), "active");
        assert_eq!(f.unit("once.service"), "inactive");
    }

    #[test]
    fn receipts_take_the_recovery_importers_bound() {
        let f = Fixture::new(0);
        // A JSON object of exactly `len` bytes.
        let receipt = |len: u64| format!(r#"{{"p":"{}"}}"#, "x".repeat(len as usize - 8));
        let private = |path: &Path, bytes: &str| {
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        let limit = super::super::recovery::MAX_OFF_HOST_RECEIPT_BYTES;
        private(&f.p("over-limit.json"), &receipt(limit + 1));
        assert!(receipt_bytes(&f.p("over-limit.json")).is_err());
        // Just over the old 1 MiB cap: read in transport and from the slot.
        let large = receipt((1 << 20) + 1);
        private(&f.p("receipt.json"), &large);
        assert_eq!(
            receipt_bytes(&f.p("receipt.json")).unwrap().len(),
            (1 << 20) + 1
        );
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        staged(&f, &rollout, "import/receipt.json", large.as_bytes());
        let error = (rollout.run(Action::Import {
            receipt: f.p("receipt.json"),
            sha256: codec::digest(large.as_bytes()),
        }))
        .unwrap_err()
        .to_string();
        // Past both reads, the (fake) worker simply does not admit it.
        assert!(error.contains("not admitted"), "{error}");
    }

    #[test]
    fn an_export_root_inside_a_writable_root_is_written_through_it() {
        let f = Fixture::new(0);
        fs::create_dir_all(f.p("vault/export/backup-1")).unwrap();
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("vault")]);
            m["prepare"]["export_root"] = json!(f.p("vault/export"));
        })
        .unwrap();
        rollout.run(FENCE).unwrap();
        // Preparation hands off from the nested export root.
        fs::write(
            f.p("evidence/preparation.json"),
            r#"{"backup_id":"backup-1"}"#,
        )
        .unwrap();
        let records = json!({"backup_id": "backup-1", "version": 1, "writer_fence_token": TOKEN});
        fs::write(f.p("evidence/records.json"), records.to_string()).unwrap();
        fs::write(f.p("vault/export/backup-1/recovery-export.json"), "{}").unwrap();
        let status = rollout.run(Action::Prepare).unwrap();
        assert_eq!(status["phase"], "prepared");
        assert_eq!(status["export"], json!(f.p("vault/export/backup-1")));
        // systemd binds the writable root: the nested export root is no
        // separate sandbox entry (whose path systemd would resolve itself).
        let property = format!(
            "--property=ReadWritePaths={} {}",
            f.p("evidence").display(),
            f.p("vault").display()
        );
        let command = rollout.preparation_command(false);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert!(args.iter().any(|arg| *arg == property), "{args:?}");
    }

    #[test]
    fn abort_waits_until_the_candidate_is_no_longer_the_boot_default() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        for phase in [Phase::Adopted, Phase::Activated] {
            let mut journal = rollout.journal().unwrap().unwrap();
            journal.phase = phase;
            rollout.save(&journal).unwrap();
            // Rolled back with `test` only: the system profile still boots it.
            std::os::unix::fs::symlink(f.p("rollout.json"), f.p("profile.json")).unwrap();
            let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
            assert!(
                error.contains("still the boot default"),
                "{phase:?}: {error}"
            );
            fs::remove_file(f.p("profile.json")).unwrap();
            std::os::unix::fs::symlink(f.p("rollout.json"), f.p("booted.json")).unwrap();
            let error = rollout.run(NO_ADOPTION).unwrap_err().to_string();
            assert!(error.contains("booted the candidate"), "{phase:?}: {error}");
            fs::remove_file(f.p("booted.json")).unwrap();
            assert_eq!(f.unit("a.service"), "inactive", "{phase:?}");
        }
        // Neither running, booting next, nor booted: it unwinds.
        assert_eq!(rollout.run(NO_ADOPTION).unwrap()["phase"], "aborted");
    }

    #[test]
    fn provisioning_never_adopts_a_directory_planted_at_its_staging_name() {
        let f = Fixture::new(0);
        let rollout = edit_manifest(&f, |m| {
            m["prepare"]["read_write_paths"] = json!([f.p("evidence"), f.p("backup")]);
            m["prepare"]["provision_directories"] = json!([
                {"path": f.p("backup"), "group": "postgres", "mode": 0o700},
                {"path": f.p("backup/nested"), "group": "postgres", "mode": 0o700},
            ]);
        })
        .unwrap();
        rollout
            .provision(&f.p("backup"), "postgres", 0o700)
            .unwrap();
        // The parent's writer renames a directory of root's to the staging
        // name it predicts: it is never chmodded, owned or published.
        let planted = f.p(&format!(
            "backup/.nested.{}.provisioning",
            std::process::id()
        ));
        fs::create_dir(&planted).unwrap();
        fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(planted.join("kept"), "").unwrap();
        rollout
            .provision(&f.p("backup/nested"), "postgres", 0o700)
            .unwrap();
        assert_eq!(fs::read_dir(f.p("backup/nested")).unwrap().count(), 0);
        assert!(planted.join("kept").exists());
        assert_eq!(fs::metadata(&planted).unwrap().mode() & 0o7777, 0o755);
        // A directory just made is adjusted only once proved fresh: root's
        // and empty, through its own descriptor.
        let parent = open_directory(&f.p("backup")).unwrap();
        fs::create_dir(f.p("backup/fresh")).unwrap();
        assert!(fresh_at(&parent, "fresh".as_ref()).is_ok());
        assert!(fresh_at(&parent, planted.file_name().unwrap()).is_err());
        std::os::unix::fs::symlink(f.p("backup/fresh"), f.p("backup/link")).unwrap();
        assert!(fresh_at(&parent, "link".as_ref()).is_err());
    }

    #[test]
    fn harbor_never_creates_the_parents_of_its_inhibition_state() {
        let f = Fixture::new(0);
        let path = f.p("maintenance.json");
        let mut maintenance = durable::read_json(&path).unwrap();
        maintenance["startup_inhibition"]["state_dir"] = json!(f.p("ctl/startup"));
        fs::write(&path, maintenance.to_string()).unwrap();
        f.rollout().run(FENCE).unwrap();
        // Root-private parents would hide sibling postgres paths from it.
        assert_eq!(fs::metadata(f.p("ctl")).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(
            fs::metadata(f.p("ctl/startup")).unwrap().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn import_never_creates_its_slot_directory_below_an_absent_mount() {
        // Declared for the worker, or only by the maintenance manifest.
        for in_maintenance in [false, true] {
            let f = Fixture::new(0);
            if in_maintenance {
                let path = f.p("maintenance.json");
                let mut maintenance = durable::read_json(&path).unwrap();
                maintenance["required_mounts"] = json!([f.p("import")]);
                fs::write(&path, maintenance.to_string()).unwrap();
            }
            let rollout = edit_manifest(&f, |m| {
                if !in_maintenance {
                    m["prepare"]["required_mounts"] = json!([f.p("import")]);
                }
            })
            .unwrap();
            let mut journal = rollout.bound().unwrap();
            journal.phase = Phase::Prepared;
            journal.fence_token = Some(TOKEN.into());
            rollout.save(&journal).unwrap();
            let receipt = f.p("receipt.json");
            fs::write(&receipt, "{}").unwrap();
            fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
            let error = rollout
                .run(Action::Import {
                    receipt,
                    sha256: codec::digest(b"{}"),
                })
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("required mount"),
                "{in_maintenance}: {error}"
            );
            assert!(!f.p("import").exists(), "{in_maintenance}");
        }
    }

    #[test]
    fn the_opening_manifests_gc_root_is_synced_around_the_journal() {
        let f = Fixture::new(0);
        // The GC root's directory is synced without following a symlink:
        // reached through one, its sync fails visibly where a skipped sync
        // would pass unnoticed.
        std::os::unix::fs::symlink(f.p("gcroots"), f.p("gcroots-link")).unwrap();
        let mut rollout = f.rollout();
        rollout.gcroots = f.p("gcroots-link");
        // The journal is never written before its GC root is durable.
        assert!(rollout.save(&rollout.bound().unwrap()).is_err());
        assert!(!f.p("state/rollout.json").exists());
        rollout.gcroots = f.p("gcroots");
        rollout.run(FENCE).unwrap();
        rollout.run(NO_ADOPTION).unwrap();
        // Archiving removes the GC root, then syncs its directory again.
        rollout.gcroots = f.p("gcroots-link");
        assert!(rollout.archive_finished().is_err());
        assert!(!f.p("gcroots").join(GC_ROOT).exists());
        assert!(!f.p("state/rollout.json").exists());
    }

    #[test]
    fn read_only_actions_are_dispatched_before_steps_require_root() {
        if root() {
            return;
        }
        let f = Fixture::new(0);
        let rollout = f.rollout();
        // Steps need root; a test never has it.
        let step = rollout.dispatch(FENCE).unwrap_err().to_string();
        assert!(step.contains("require root"), "{step}");
        // Status and the activation check are answered by their own handlers
        // (a misrouted one would meet that refusal, never `run`).
        assert_eq!(rollout.dispatch(Action::Status).unwrap()["phase"], "idle");
        assert_eq!(
            rollout.dispatch(Action::ActivationCheck).unwrap()["phase"],
            "idle"
        );
        // Between fence and adoption the check refuses for its own reason.
        rollout.run(FENCE).unwrap();
        let error = rollout
            .dispatch(Action::ActivationCheck)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("require root"), "{error}");
        assert!(error.contains("between fence and adoption"), "{error}");
    }

    #[test]
    fn activation_is_refused_between_fence_and_adoption() {
        let f = Fixture::new(0);
        let rollout = f.rollout();
        assert!(rollout.activation_check().is_ok(), "no rollout");
        rollout.run(FENCE).unwrap();
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
        // Adopted or activated, the rollout can only be thawed by its own
        // manifest: switching to another generation strands it.
        for phase in [Phase::Adopted, Phase::Activated] {
            journal.phase = phase;
            rollout.save(&journal).unwrap();
            assert!(rollout.activation_check().is_ok(), "{phase:?}");
            let error = other.activation_check().unwrap_err().to_string();
            assert!(error.contains("adopted under"), "{phase:?}: {error}");
        }
        journal.phase = Phase::Aborted;
        rollout.save(&journal).unwrap();
        assert!(other.activation_check().is_err(), "an unwinding abort");
        journal.reopened = true;
        for phase in [Phase::Thawed, Phase::Aborted] {
            journal.phase = phase;
            rollout.save(&journal).unwrap();
            assert!(other.activation_check().is_ok(), "{phase:?}");
        }
    }

    #[test]
    fn a_generation_without_the_coordinator_never_strands_an_open_rollout() {
        let f = Fixture::new(0);
        let state = f.p("state");
        assert_eq!(guard_at(&state).unwrap()["status"], "clear", "no journal");
        let rollout = f.rollout();
        rollout.run(FENCE).unwrap();
        let mut journal = rollout.journal().unwrap().unwrap();
        let mut refused = |phase: Phase, reopened: bool| {
            journal.phase = phase;
            journal.reopened = reopened;
            rollout.save(&journal).unwrap();
            guard_at(&state).is_err()
        };
        for phase in [Phase::Idle, Phase::Fenced, Phase::Prepared, Phase::Imported] {
            assert!(refused(phase, false), "{phase:?}");
        }
        // An adopted or activated rollout may be rolled back (abort then
        // unwinds it), but not while it is already unwinding.
        for phase in [Phase::Adopted, Phase::Activated] {
            assert!(!refused(phase, false), "{phase:?}");
            assert!(refused(phase, true), "unwinding {phase:?}");
        }
        assert!(refused(Phase::Aborted, false), "an unwinding abort");
        assert!(!refused(Phase::Aborted, true));
        assert!(!refused(Phase::Thawed, true));
        // A never-started journal lets any generation in.
        let fresh = rollout.fresh_journal();
        rollout.save(&fresh).unwrap();
        assert_eq!(guard_at(&state).unwrap()["phase"], "idle");
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
        rollout.run(FENCE).unwrap();
        // The thaw closed the fence and reopened under the old manifest.
        let mut journal = rollout.journal().unwrap().unwrap();
        journal.phase = Phase::Thawed;
        journal.reopened = true;
        rollout.save(&journal).unwrap();
        fs::remove_file(f.p("authority/writer-fence.json")).unwrap();
        let next = f.load(&f.manifest("next.json", 5)).unwrap();
        let status = next.run(FENCE).unwrap();
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

    /// A manifest whose executor is this process, with a fake certify that
    /// chatters on stdout and writes `{}` as its receipt.
    fn executor_rollout(f: &Fixture) -> Rollout {
        executable(
            &f.p("certify"),
            "echo EXECUTOR-CHATTER\nwhile [ $# -gt 0 ]; do [ \"$1\" = --workspace ] && w=$2; shift; done\nmkdir -p \"$w\"; printf '{}' > \"$w/off-host-recovery.json\"",
        );
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        let user = process::current_username().unwrap();
        edit_manifest(
            f,
            |m| {
                m["executor"] =
                    json!({"host": host.trim(), "user": user, "command": [f.p("certify")]})
            },
        )
        .unwrap()
    }

    #[test]
    fn certify_reports_the_receipt_digest_import_checks() {
        if root() {
            return;
        }
        let f = Fixture::new(0);
        let rollout = executor_rollout(&f);
        let certified = rollout
            .certify(&f.p("export"), &"a".repeat(64), &f.p("workspace"))
            .unwrap();
        let receipt = codec::digest(b"{}");
        assert_eq!(certified["receipt_sha256"], receipt);
        let next = certified["next"].as_str().unwrap();
        assert!(next.contains(&format!("--sha256 {receipt}")), "{next}");
    }

    /// Steps print exactly one status document, so worker and executor output
    /// must reach stderr. Re-executes this test to observe the real descriptors.
    #[test]
    fn worker_and_executor_output_never_reaches_step_stdout() {
        const CHILD: &str = "HARBOR_DB_ROLLOUT_STDOUT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let f = Fixture::new(0);
            executable(&f.p("systemd-run"), "echo WORKER-CHATTER; exit 1");
            let rollout = executor_rollout(&f);
            assert!(!worker_ready(&rollout, &mut rollout.bound().unwrap()).unwrap());
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
        f.rollout().run(FENCE).unwrap();
        fs::set_permissions(f.p("state"), fs::Permissions::from_mode(0o000)).unwrap();
        let status = f.rollout().status();
        fs::set_permissions(f.p("state"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(status.is_err());
    }
}
