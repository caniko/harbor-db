//! First-rollout coordinator for a guarded PostgreSQL primary. It only sequences
//! the existing Harbor verbs (startup inhibition, writer fence, preparation,
//! adoption, thaw) behind one resumable journal; every path, unit and executor
//! comes from the immutable rollout manifest.
use super::{Result, codec, durable, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PAUSE_DROP_IN: &str = "zz-harbor-db-rollout-pause.conf";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub host: String,
    pub system_identifier: String,
    pub harbor: PathBuf,
    pub systemctl: PathBuf,
    pub runuser: PathBuf,
    pub systemd_run: PathBuf,
    /// Harbor's fenced maintenance manifest (data_dir, recovery, startup_inhibition).
    pub maintenance: PathBuf,
    /// Harbor's `prepare-recovery --preparation-config` argv file.
    pub preparation: PathBuf,
    pub state_dir: PathBuf,
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
}

pub enum Action {
    Status,
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
    Abort,
}

pub struct Rollout {
    pub manifest: Manifest,
    pub maintenance: Value,
    pub digest: String,
    config: PathBuf,
}

fn root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn operate(config: &Path, action: Action) -> Result<Value> {
    let rollout = Rollout::load(&durable::immutable_config_path(config)?)?;
    match action {
        Action::Status => rollout.status(),
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

fn checked(command: &mut Command) -> Result<Output> {
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        return Err(invalid(format!(
            "rollout command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

fn response(command: &mut Command, status: &str) -> Result<Value> {
    let value = codec::decode(&checked(command)?.stdout)?;
    if value["status"] != status {
        return Err(invalid(format!(
            "unexpected Harbor response; expected {status}"
        )));
    }
    Ok(value)
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
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o777 != 0o600
        || !parent.is_dir()
        || parent.uid() != owner
        || parent.mode() & 0o022 != 0
    {
        return Err(invalid(format!(
            "{} must be a mode-0600 file owned by uid {owner} in a directory only that owner can modify",
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
        if manifest.version != 1
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
        {
            return Err(invalid("invalid rollout manifest or primary binding"));
        }
        let state = manifest.state_dir.as_path();
        let export = manifest.prepare.export_root.as_path();
        if !dedicated(state) || !dedicated(export) || overlaps(state, export) {
            return Err(invalid(
                "rollout state and export roots must be distinct dedicated directories",
            ));
        }
        for protected in ["data_dir", "state_dir"] {
            let path = Path::new(super::string(&maintenance, protected)?);
            if overlaps(state, path) || overlaps(export, path) {
                return Err(invalid(
                    "rollout state or export root overlaps primary/authority state",
                ));
            }
        }
        // Thaw runs the checker as root: only immutable store inputs qualify.
        if let Some(cutover) = &manifest.cutover {
            for path in [&cutover.checker, &cutover.contract] {
                if !path.starts_with("/nix/store") || !dedicated(path) {
                    return Err(invalid("cutover checker and contract must be store paths"));
                }
            }
        }
        Ok(Self {
            digest: codec::file_digest(config)?,
            maintenance,
            manifest,
            config: config.to_owned(),
        })
    }

    fn journal_path(&self) -> PathBuf {
        self.manifest.state_dir.join("rollout.json")
    }

    fn journal(&self) -> Result<Option<Journal>> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(None);
        }
        let journal: Journal = serde_json::from_value(durable::read_json(&path)?)?;
        if journal.version != 1 || journal.system_identifier != self.manifest.system_identifier {
            return Err(invalid(
                "retained rollout journal belongs to another primary",
            ));
        }
        Ok(Some(journal))
    }

    /// Steps are bound to the exact manifest; only `abort` may unwind a journal
    /// opened by another manifest of the same primary.
    fn bound(&self) -> Result<Journal> {
        let journal = self.journal()?.unwrap_or(Journal {
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
        });
        if journal.manifest_sha256 != self.digest {
            return Err(invalid(
                "retained rollout journal belongs to another manifest; finish or abort it first",
            ));
        }
        Ok(journal)
    }

    /// Create the state directory privately, but never re-permission an
    /// existing one: it must already be a private directory this process owns.
    fn state_dir(&self) -> Result<()> {
        let state = &self.manifest.state_dir;
        match fs::symlink_metadata(state) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
                    return Err(invalid(
                        "rollout state directory must be a private directory owned by the coordinator",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(state)?;
                fs::set_permissions(state, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        self.state_dir()?;
        durable::write_json(&self.journal_path(), &serde_json::to_value(journal)?)
    }

    fn harbor(&self, as_postgres: bool) -> Command {
        let mut command = if as_postgres {
            let mut command = Command::new(&self.manifest.runuser);
            command
                .args(["-u", "postgres", "--"])
                .arg(&self.manifest.harbor);
            command
        } else {
            Command::new(&self.manifest.harbor)
        };
        command.arg("--config").arg(&self.manifest.maintenance);
        command
    }

    fn systemctl(&self) -> Command {
        Command::new(&self.manifest.systemctl)
    }

    fn primary_unit(&self) -> &str {
        self.maintenance["startup_inhibition"]["unit"]
            .as_str()
            .unwrap_or("postgresql.service")
    }

    fn show(&self, unit: &str, property: &str) -> Result<String> {
        let output = checked(
            self.systemctl()
                .args(["show", unit, "--value"])
                .arg(format!("--property={property}")),
        )?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn active_clients(&self) -> Result<Vec<String>> {
        let output = checked(self.systemctl().args([
            "list-units",
            "--type=service",
            "--state=active,activating,reloading,deactivating",
            "--plain",
            "--no-legend",
            "--no-pager",
        ]))?;
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

    fn stop(&self, unit: &str) -> Result<()> {
        checked(self.systemctl().args(["stop", "--", unit]))?;
        match self.show(unit, "ActiveState")?.as_str() {
            "inactive" | "failed" => Ok(()),
            state => Err(invalid(format!("{unit} did not stop ({state})"))),
        }
    }

    /// Start the primary, then wait for the declared setup units: they connect
    /// as the control role right after startup and must not race inspection.
    fn start_primary(&self) -> Result<()> {
        checked(self.systemctl().args(["start", "--", self.primary_unit()]))?;
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
        let value = response(
            self.harbor(false).args([
                "inhibit-startup",
                "--system-identifier",
                &self.manifest.system_identifier,
            ]),
            "startup-inhibited",
        )?;
        journal.startup_token = Some(super::string(&value, "token")?.to_owned());
        self.save(journal)
    }

    fn release_startup(&self, journal: &Journal, phase: &str) -> Result<()> {
        response(
            self.harbor(false)
                .args(["release-startup", "--token"])
                .arg(journal.startup_token.as_deref().unwrap_or_default())
                .arg("--fence-token")
                .arg(journal.fence_token.as_deref().unwrap_or_default())
                .args(["--phase", phase]),
            "startup-released",
        )?;
        Ok(())
    }

    fn inspect_fence(&self, journal: &Journal) -> Result<()> {
        let value = response(
            self.harbor(true)
                .args(["inspect-fence", "--token"])
                .arg(journal.fence_token.as_deref().unwrap_or_default())
                .arg("--socket-dir")
                .arg(&self.manifest.socket_dir)
                .args(["--port", &self.manifest.port.to_string()]),
            "ready",
        )?;
        if value["system_identifier"] != self.manifest.system_identifier.as_str() {
            return Err(invalid("live fence readback differs"));
        }
        Ok(())
    }

    fn pause_marker(&self) -> PathBuf {
        self.manifest.state_dir.join("paused")
    }

    fn drop_in_root(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(super::string(
            &self.maintenance["startup_inhibition"],
            "drop_in_root",
        )?))
    }

    /// Publishers are skipped while the marker exists; the drop-in survives
    /// daemon-reload and generation switches, unlike a stopped timer.
    fn pause_publishers(&self) -> Result<()> {
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
        checked(self.systemctl().arg("daemon-reload"))?;
        for unit in &self.manifest.paused_publishers {
            self.stop(unit)?;
        }
        Ok(())
    }

    fn lift_pause(&self) -> Result<()> {
        if self.manifest.paused_publishers.is_empty() {
            return Ok(());
        }
        let root = self.drop_in_root()?;
        for unit in &self.manifest.paused_publishers {
            let file = root.join(format!("{unit}.d")).join(PAUSE_DROP_IN);
            if file.exists() {
                fs::remove_file(file)?;
            }
        }
        if self.pause_marker().exists() {
            fs::remove_file(self.pause_marker())?;
        }
        checked(self.systemctl().arg("daemon-reload"))?;
        Ok(())
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

    fn provision(&self, path: &Path, group: &str, mode: u32) -> Result<()> {
        if !path.exists() {
            let (uid, _) = account("postgres")?;
            let gid = group_id(group)?;
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
        }
        if !fs::metadata(path)?.is_dir() {
            return Err(invalid(format!("{} is not a directory", path.display())));
        }
        Ok(())
    }

    fn fence(&self, journal: &mut Journal) -> Result<()> {
        if journal.phase != Phase::Idle {
            // Resumed: re-stop any newly started declared client and re-verify.
            self.stop_clients(journal)?;
            return self.inspect_fence(journal);
        }
        let authority = Path::new(super::string(&self.maintenance, "state_dir")?);
        self.provision(authority, "postgres", 0o700)?;
        self.inhibit(journal)?;
        self.pause_publishers()?;
        self.stop_clients(journal)?;
        self.stop(self.primary_unit())?;
        let value = response(
            self.harbor(true).args([
                "fence-open",
                "--system-identifier",
                &self.manifest.system_identifier,
            ]),
            "prepared-offline",
        )?;
        journal.fence_token = Some(super::string(&value, "token")?.to_owned());
        self.save(journal)?;
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

    fn preparation_command(&self, unit: &str, import: bool) -> Command {
        let prepare = &self.manifest.prepare;
        let mut command = Command::new(&self.manifest.systemd_run);
        command
            .args(["--quiet", "--wait", "--pipe", "--collect"])
            .arg(format!("--unit={unit}"))
            .args([
                "--property=Type=oneshot",
                "--property=User=postgres",
                "--property=Group=postgres",
                "--property=ProtectSystem=strict",
                "--property=PrivateTmp=true",
                "--property=NoNewPrivileges=true",
                "--property=UMask=0077",
                "--property=KillMode=control-group",
                "--property=TimeoutStartSec=infinity",
                "--property=CPUWeight=10",
            ]);
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
        command
    }

    /// Run Harbor's preparation with the installed receivers stopped; they are
    /// restored whatever the outcome. Returns whether preparation reported ready.
    fn run_preparation(&self, import: bool) -> Result<bool> {
        for directory in &self.manifest.prepare.provision_directories {
            self.provision(&directory.path, &directory.group, directory.mode)?;
        }
        let mut resume = Vec::new();
        let quiesced = (|| {
            for unit in &self.manifest.receiver_units {
                // A transitional receiver can still be writing WAL; quiesce every
                // non-terminal state, and resume those that were (going) up.
                let state = self.show(unit, "ActiveState")?;
                if !matches!(state.as_str(), "inactive" | "failed") {
                    if state != "deactivating" {
                        resume.push(unit);
                    }
                    self.stop(unit)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = quiesced {
            for unit in &resume {
                let _ = self.systemctl().args(["start", "--", unit]).output();
            }
            return Err(error);
        }
        let unit = format!("harbor-db-rollout-prepare-{}", std::process::id());
        let status = self.preparation_command(&unit, import).status();
        let _ = self
            .systemctl()
            .args(["stop", &format!("{unit}.service")])
            .output();
        for unit in resume {
            checked(self.systemctl().args(["start", "--", unit]))?;
        }
        Ok(status?.success())
    }

    fn prepare(&self, journal: &mut Journal) -> Result<()> {
        self.inspect_fence(journal)?;
        let import = self.import_slot(journal)?;
        if self.run_preparation(import)? {
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
            let verified = self.manifest.state_dir.join("receipt.verified.json");
            durable::atomic_write_mode(&verified, &bytes, 0o600)?;
            super::recovery::stage_off_host_import(&verified, slot)?;
        }
        journal.receipt_sha256 = Some(expected);
        self.save(journal)?;
        self.inspect_fence(journal)?;
        if !self.run_preparation(true)? {
            return Err(invalid(
                "independent receipt was not admitted; retain the writer window",
            ));
        }
        journal.phase = Phase::Imported;
        self.save(journal)
    }

    fn adopt(&self, journal: &mut Journal) -> Result<()> {
        let socket = self.manifest.socket_dir.display().to_string();
        let port = self.manifest.port.to_string();
        // Live records must still equal the accepted snapshot before authority.
        response(
            self.harbor(true)
                .args(["inspect-recovery", "--socket-dir", &socket, "--port", &port]),
            "ready",
        )?;
        checked(self.harbor(true).args([
            "adopt-live",
            "--system-identifier",
            &self.manifest.system_identifier,
            "--socket-dir",
            &socket,
            "--port",
            &port,
        ]))?;
        checked(self.harbor(true).arg("check"))?;
        journal.phase = Phase::Adopted;
        self.save(journal)
    }

    /// Unwind to the normal primary: close (or confirm closed) the fence under
    /// startup exclusion, restart, verify, then return recorded clients.
    fn reopen(&self, journal: &mut Journal) -> Result<()> {
        if journal.fence_token.is_some() {
            self.inhibit(journal)?;
            self.stop(self.primary_unit())?;
            let token = journal.fence_token.clone().unwrap_or_default();
            let closed = self
                .harbor(true)
                .args([
                    "inspect-offline-fence",
                    "--token",
                    &token,
                    "--phase",
                    "closed",
                ])
                .stdin(Stdio::null())
                .output()?;
            if !closed.status.success() {
                response(
                    self.harbor(true).args(["fence-close", "--token", &token]),
                    "closed-offline",
                )?;
            }
            self.release_startup(journal, "closed")?;
            self.start_primary()?;
            let live = codec::decode(
                &checked(
                    self.harbor(true)
                        .args([
                            "inspect-live",
                            "--system-identifier",
                            &self.manifest.system_identifier,
                            "--socket-dir",
                        ])
                        .arg(&self.manifest.socket_dir)
                        .args(["--port", &self.manifest.port.to_string()]),
                )?
                .stdout,
            )?;
            if live["system_identifier"] != self.manifest.system_identifier.as_str() {
                return Err(invalid("normal primary readback differs"));
            }
        } else if journal.startup_token.is_some() {
            // ponytail: an acquisition interrupted between inhibition and
            // fence-open needs manual release; Harbor binds release to a fence.
            return Err(invalid(
                "startup is inhibited without an open fence; release it manually",
            ));
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
            checked(self.systemctl().args(["start", "--", unit]))?;
        }
        self.lift_pause()
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
                response(self.harbor(true).arg("inspect-recovery"), "ready")?;
                if let Some(cutover) = &self.manifest.cutover {
                    checked(
                        Command::new(&cutover.checker)
                            .arg("check")
                            .arg("--contract")
                            .arg(&cutover.contract)
                            .args(["--host", &self.manifest.host, "--phase", "certify"]),
                    )?;
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
        match journal.phase {
            Phase::Prepared => json!(format!(
                "on {} as {}: harbor-db-postgres --config {config} rollout certify --export <copied {}> --export-sha256 {} --workspace <fresh dir>; then `rollout import --receipt <receipt> --sha256 <sha>`",
                self.manifest.executor.host,
                self.manifest.executor.user,
                journal
                    .export
                    .as_deref()
                    .map(Path::display)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
                journal.export_sha256.as_deref().unwrap_or_default(),
            )),
            Phase::Adopted => {
                json!("activate the candidate generation, then run `rollout activated`")
            }
            Phase::Activated => {
                json!("verify the deployment, then run `rollout thaw --acceptance <receipt>`")
            }
            Phase::Thawed | Phase::Aborted => json!("complete"),
            _ => json!("run `rollout next`"),
        }
    }

    fn status(&self) -> Result<Value> {
        Ok(match self.journal()? {
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
        })
    }

    pub fn run(&self, action: Action) -> Result<Value> {
        // ponytail: one exclusive lease per rollout; steps are idempotent, so an
        // interrupted step is resumed by rerunning it rather than by signal traps.
        self.state_dir()?;
        let _lease = durable::lock(&self.manifest.state_dir.join("rollout.lock"), false, true)?;
        if let Action::Abort = action {
            let Some(mut journal) = self.journal()? else {
                return Err(invalid("no rollout to abort"));
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
            return self.status();
        }
        let mut journal = self.bound()?;
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
                self.inspect_fence(&journal)?;
                journal.phase = Phase::Activated;
                self.save(&journal)?;
            }
            Action::Thaw { acceptance } => {
                require(&[Phase::Activated, Phase::Thawed])?;
                if phase == Phase::Activated {
                    self.thaw(&mut journal, &acceptance)?;
                }
            }
            Action::Next => match phase {
                Phase::Idle => self.fence(&mut journal)?,
                Phase::Fenced => self.prepare(&mut journal)?,
                Phase::Imported => self.adopt(&mut journal)?,
                _ => {}
            },
            Action::Status | Action::Certify { .. } | Action::Abort => unreachable!(),
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
            for dir in ["units", "authority", "data", "evidence", "control", "state"] {
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
  start) echo active > "$u/$3"; [ "$3" = postgresql.service ] && echo {polls} > "$u/.setup-polls" ;;
  show) prop="$4"; unit="$2"
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
            executable(&p("runuser"), "shift 3\nexec \"$@\"");
            // The preparation worker always reports "not ready".
            executable(&p("systemd-run"), "exit 1");
            executable(
                &p("harbor"),
                &format!(
                    r#"log={log}; shift 2; echo "harbor $*" >> "$log"
case "$1" in
  inhibit-startup) echo '{{"status":"startup-inhibited","token":"{TOKEN}"}}' ;;
  fence-open) echo '{{"status":"prepared-offline","token":"{TOKEN}","restart_required":true}}' ;;
  release-startup) echo '{{"status":"startup-released"}}' ;;
  inspect-fence) echo '{{"status":"ready","token":"{TOKEN}","system_identifier":"42"}}' ;;
  inspect-offline-fence) exit 1 ;;
  fence-close) echo '{{"status":"closed-offline","token":"{TOKEN}"}}' ;;
  inspect-live) echo '{{"system_identifier":"42"}}' ;;
  inspect-recovery) echo '{{"status":"ready"}}' ;;
  *) echo '{{}}' ;;
esac"#,
                    log = log.display(),
                ),
            );
            let maintenance = json!({
                "data_dir": p("data"), "state_dir": p("authority"),
                "recovery": {"system_identifier": "42", "require_writer_fence": true,
                    "snapshot_file": p("evidence/records.json")},
                "startup_inhibition": {"unit": "postgresql.service", "drop_in_root": p("control")},
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
                "version": 1, "host": "primary", "system_identifier": "42",
                "harbor": self.p("harbor"), "systemctl": self.p("systemctl"),
                "runuser": self.p("runuser"), "systemd_run": self.p("systemd-run"),
                "maintenance": self.p("maintenance.json"), "preparation": self.p("preparation.json"),
                "state_dir": self.p("state"), "socket_dir": "/run/postgresql", "port": 5432,
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
            Rollout::load(&self.p("rollout.json")).unwrap()
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

        let status = f.rollout().run(Action::Abort).unwrap();
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

        let other = Rollout::load(&f.manifest("other.json", 5)).unwrap();
        let error = other.run(Action::Prepare).unwrap_err().to_string();
        assert!(error.contains("another manifest"), "{error}");
        // A different manifest of the same primary may still unwind the fence.
        assert_eq!(other.run(Action::Abort).unwrap()["phase"], "aborted");
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
        let error = rollout.run(Action::Abort).unwrap_err().to_string();
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
            .preparation_command("u", false)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .any(|arg| arg.starts_with("--property=ReadWritePaths=") && arg.contains(&export));
        assert!(writable, "export root must be writable by the worker");
        assert!(!rollout.run_preparation(false).unwrap());
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
            fs::Permissions::from_mode(0o600),
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
        fs::write(f.p("import/receipt.json"), br#"{"status":"other"}"#).unwrap();
        let error = import();
        assert!(error.contains("different receipt"), "{error}");
    }

    fn edit_manifest(f: &Fixture, edit: impl FnOnce(&mut Value)) -> Result<Rollout> {
        let path = f.p("rollout.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut manifest);
        fs::write(&path, manifest.to_string()).unwrap();
        Rollout::load(&path)
    }

    #[test]
    fn load_rejects_shared_overlapping_or_mutable_rollout_inputs() {
        let f = Fixture::new(0);
        for (field, value) in [
            ("state_dir", json!("/etc")),
            ("state_dir", json!(f.p("data").join("rollout"))),
            ("state_dir", json!(f.p("export"))),
        ] {
            let error = edit_manifest(&f, |m| m[field] = value.clone())
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("state"),
                "{field}={value}: {error}"
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
        assert!(rollout.run_preparation(false).is_err());
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
    }
}
