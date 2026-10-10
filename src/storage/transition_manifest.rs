//! Immutable transition intent, persistent startup barriers and inherited worker leases.
use super::{
    Result, codec,
    custody::{self, array, paths},
    durable::{self, Lease},
    invalid, process, resource, string,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::{
        fd::RawFd,
        unix::fs::{DirBuilderExt, MetadataExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(invalid("persistent startup inhibition requires root"));
    }
    Ok(())
}
pub fn generation() -> Result<String> {
    let path = Path::new("/run/current-system");
    Ok(resolve(path)?.to_string_lossy().into_owned())
}
fn resolve(path: &Path) -> Result<PathBuf> {
    resolve_bounded(path, 0)
}
fn resolve_bounded(path: &Path, depth: usize) -> Result<PathBuf> {
    if depth > 256 {
        return Err(invalid("transition path contains a symbolic-link cycle"));
    }
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    if let Ok(target) = fs::read_link(path) {
        return resolve_bounded(
            &if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(Path::new("/")).join(target)
            },
            depth + 1,
        );
    }
    let parent = path.parent().ok_or_else(|| invalid("invalid path"))?;
    let base = if parent == path {
        parent.to_path_buf()
    } else {
        resolve_bounded(parent, depth + 1)?
    };
    Ok(base.join(path.file_name().ok_or_else(|| invalid("invalid path"))?))
}
pub fn generation_contract(
    candidate: &Path,
    resource_name: &str,
    store_root: &Path,
) -> Result<Value> {
    let path = fs::canonicalize(
        candidate
            .join("etc/harbor-db")
            .join(format!("{resource_name}-transition.json")),
    )?;
    if !path.starts_with(store_root) {
        return Err(invalid(
            "candidate transition contract escapes the immutable store",
        ));
    }
    durable::read_json(&path)
}
pub fn candidate_path(candidate: &str, config: &Value) -> Result<&'static str> {
    let valid = candidate.strip_prefix("/nix/store/").is_some_and(|s| {
        s.len() > 33
            && s.as_bytes()[32] == b'-'
            && s.as_bytes()[..32]
                .iter()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
            && !s[33..].is_empty()
            && !s[33..].chars().any(|c| c == '/' || c.is_whitespace())
    });
    if !valid {
        return Err(invalid(
            "transition candidate must be an immutable store contract or generation",
        ));
    }
    let path = Path::new(candidate);
    if path.is_file() {
        if durable::read_config_json(path)? != *config {
            return Err(invalid("immutable preparation contract differs"));
        }
        return Ok("contract");
    }
    if !path.is_dir() {
        return Err(invalid("transition candidate is not realized"));
    }
    if generation_contract(path, string(config, "resource")?, Path::new("/nix/store"))? != *config {
        return Err(invalid(
            "candidate generation does not declare the exact transition contract",
        ));
    }
    Ok("generation")
}
pub fn owned(path: &Path, directory: bool) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if (if directory { !m.is_dir() } else { !m.is_file() })
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
    {
        return Err(invalid(
            "startup inhibition requires owned non-writable storage",
        ));
    }
    Ok(())
}
pub fn owned_ancestors(path: &Path) -> Result<()> {
    let root_uid = durable::root_owner_uid()?;
    for parent in path.ancestors().skip(1) {
        if parent.exists() {
            let m = fs::symlink_metadata(parent)?;
            let sticky = m.uid() == root_uid && m.mode() & libc::S_ISVTX != 0;
            if !m.is_dir()
                || (m.uid() != root_uid && m.uid() != unsafe { libc::geteuid() })
                || m.mode() & 0o022 != 0 && !sticky
            {
                return Err(invalid(
                    "startup inhibition storage has an untrusted ancestor",
                ));
            }
        }
    }
    Ok(())
}
pub fn validate(config: &Value) -> Result<(Value, Value)> {
    let required = [
        "version",
        "resource",
        "source_manifest",
        "target_manifest",
        "barrier_dir",
        "drop_in_root",
        "systemctl",
        "busctl",
        "units",
        "retired_units",
        "timeout_seconds",
        "commands",
        "executable_files",
        "postgres_manifest",
        "postgres_socket",
        "postgres_port",
        "custody_manifest",
        "backup_manifest",
        "independent_receipt",
        "storage_package",
        "runuser",
    ];
    let keys = config
        .as_object()
        .ok_or_else(|| invalid("unsupported application transition manifest"))?;
    if keys.len() != required.len()
        || required.iter().any(|k| !keys.contains_key(*k))
        || config["version"] != 1
    {
        return Err(invalid("unsupported application transition manifest"));
    }
    let source = durable::read_config_json(Path::new(string(config, "source_manifest")?))?;
    let target = durable::read_config_json(Path::new(string(config, "target_manifest")?))?;
    if source["resource"] != config["resource"]
        || target["resource"] != config["resource"]
        || source["state_dir"] != target["state_dir"]
        || source["binding"] == target["binding"]
    {
        return Err(invalid(
            "backend transitions require one authority and different source/target bindings",
        ));
    }
    if [
        source["binding"]["backend"].as_str(),
        target["binding"]["backend"].as_str(),
    ]
    .iter()
    .any(|v| matches!(v, Some("postgres" | "postgresql")))
        && config["postgres_manifest"].is_null()
    {
        return Err(invalid(
            "PostgreSQL backend transitions require the existing writer fence",
        ));
    }
    for key in [
        "source_manifest",
        "target_manifest",
        "barrier_dir",
        "drop_in_root",
        "systemctl",
        "busctl",
        "backup_manifest",
        "independent_receipt",
        "storage_package",
        "runuser",
    ] {
        let s = string(config, key)?;
        if !Path::new(s).is_absolute()
            || s.chars()
                .any(|c| c.is_whitespace() || c == '\\' || c == '%')
        {
            return Err(invalid(
                "transition paths must be explicit, absolute and systemd-safe",
            ));
        }
    }
    let barrier = Path::new(string(config, "barrier_dir")?);
    let authority = Path::new(string(&source, "state_dir")?);
    let drop_in = Path::new(string(config, "drop_in_root")?);
    if resolve(barrier)? != barrier || resolve(drop_in)? != drop_in {
        return Err(invalid("transition barrier storage is redirected"));
    }
    owned_ancestors(barrier)?;
    if barrier.exists() {
        owned(barrier, true)?;
    }
    if barrier.starts_with(authority) || authority.starts_with(barrier) {
        return Err(invalid(
            "root startup barriers must be outside application-owned authority",
        ));
    }
    for p in paths(&source, "directories")?
        .iter()
        .chain(paths(&target, "directories")?.iter())
    {
        if barrier.starts_with(p) {
            return Err(invalid(
                "startup barriers must be outside the guarded storage",
            ));
        }
    }
    let units = units(config)?;
    let unique: BTreeSet<_> = units.iter().collect();
    if units.is_empty()
        || unique.len() != units.len()
        || units.iter().any(|u| {
            !custody::unit(u)
                || matches!(
                    u.as_str(),
                    "postgresql.service" | "postgresql-setup.service"
                )
        })
    {
        return Err(invalid(
            "application transition units must be unique clients, not PostgreSQL control services",
        ));
    }
    if !config["timeout_seconds"]
        .as_u64()
        .is_some_and(|v| v > 0 && v <= 86400)
    {
        return Err(invalid("transition execution limit must be bounded"));
    }
    if !Path::new(string(config, "postgres_socket")?).is_absolute()
        || !config["postgres_port"]
            .as_u64()
            .is_some_and(|n| (1..=65535).contains(&n))
    {
        return Err(invalid(
            "transition requires an explicit local PostgreSQL endpoint",
        ));
    }
    let commands = config["commands"]
        .as_object()
        .ok_or_else(|| invalid("invalid transition commands"))?;
    if commands.len() != 4
        || ["import", "verify-target", "verify-source", "health"]
            .iter()
            .any(|k| !commands.contains_key(*k))
    {
        return Err(invalid(
            "transition requires import, complete parity, source validation and health commands",
        ));
    }
    for c in commands.values() {
        let obj = c
            .as_object()
            .ok_or_else(|| invalid("invalid transition worker"))?;
        let argv = array(c, "argv")?;
        if obj.len() != 2
            || !obj.contains_key("user")
            || argv.is_empty()
            || argv.iter().any(|v| !v.is_string())
            || !Path::new(argv[0].as_str().unwrap_or("")).is_absolute()
        {
            return Err(invalid(
                "transition workers require a declared user and absolute argv",
            ));
        }
        process::account(string(c, "user")?)?;
    }
    Ok((source, target))
}
/// Declared immutable policy/tools may be Nix package links. Resolve their target
/// before opening it; corpus and recovery evidence retain no-follow semantics.
pub fn declared_digest(path: &Path) -> Result<String> {
    codec::file_digest(&fs::canonicalize(path)?)
}
pub fn tools(config: &Value) -> Result<Value> {
    let mut files: Vec<PathBuf> = paths(config, "executable_files")?;
    for command in config["commands"]
        .as_object()
        .ok_or_else(|| invalid("invalid commands"))?
        .values()
    {
        files.push(PathBuf::from(
            array(command, "argv")?[0]
                .as_str()
                .ok_or_else(|| invalid("invalid executable"))?,
        ));
    }
    for key in ["systemctl", "busctl", "runuser"] {
        files.push(PathBuf::from(string(config, key)?));
    }
    files.push(Path::new(string(config, "storage_package")?).join("harbor-db-application-backup"));
    files.sort();
    files.dedup();
    let mut result = serde_json::Map::new();
    for file in files {
        if !file.is_absolute() || !file.is_file() {
            return Err(invalid("transition executable is absent"));
        }
        result.insert(
            file.to_string_lossy().into_owned(),
            json!(declared_digest(&file)?),
        );
    }
    Ok(Value::Object(result))
}
pub fn intent(config: &Value) -> Result<Value> {
    validate(config)?;
    let hash = |k| -> Result<Value> {
        if config[k].is_null() {
            Ok(Value::Null)
        } else {
            Ok(json!(declared_digest(Path::new(string(config, k)?))?))
        }
    };
    Ok(
        json!({"manifest_sha256":codec::digest(&codec::encode(config,true)?),"source_sha256":hash("source_manifest")?,"target_sha256":hash("target_manifest")?,"backup_sha256":hash("backup_manifest")?,"custody_sha256":hash("custody_manifest")?,"postgres_sha256":hash("postgres_manifest")?,"executables":tools(config)?}),
    )
}
pub fn write_owned(path: &Path, value: &Value) -> Result<()> {
    let parent = fs::metadata(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
    durable::write_json(path, value)?;
    use std::os::fd::AsRawFd;
    let file = durable::open_regular(path, false)?;
    if unsafe { libc::fchown(file.as_raw_fd(), parent.uid(), parent.gid()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    file.sync_all()?;
    Ok(())
}
fn units(config: &Value) -> Result<Vec<String>> {
    array(config, "units")?
        .iter()
        .chain(array(config, "retired_units")?)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("invalid service unit"))
        })
        .collect()
}
fn provision_directory(path: &Path, mode: u32) -> Result<()> {
    let missing: Vec<_> = path
        .ancestors()
        .take_while(|p| !p.exists())
        .map(Path::to_path_buf)
        .collect();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)?;
    owned(path, true)?;
    for p in missing {
        durable::sync_directory(&p)?;
        durable::sync_directory(p.parent().ok_or_else(|| invalid("missing parent"))?)?;
    }
    Ok(())
}
pub fn transition_content(config: &Value, selected: &Path, unit: &str) -> Result<Vec<u8>> {
    Ok(format!("# Harbor DB: retained across legacy and guarded generations.\n[Unit]\nConditionPathExists=!{}\n\n[Service]\nExecCondition=+{} --state {} --unit {unit}\n",selected.join("inhibited.json").display(),Path::new(string(config,"storage_package")?).join("harbor-db-transition-start").display(),string(config,"barrier_dir")?).into_bytes())
}
pub fn check_transition_drop_in(
    config: &Value,
    path: &Path,
    selected: &Path,
    unit: &str,
) -> Result<()> {
    owned(path, false)?;
    if fs::read(path)? != transition_content(config, selected, unit)? {
        return Err(invalid(
            "persistent transition startup policy changed; preserve foreign policy",
        ));
    }
    Ok(())
}
fn call(config: &Value, key: &str, args: Vec<String>, seconds: u64) -> Result<Vec<u8>> {
    let mut spec = process::CommandSpec::new(
        std::iter::once(string(config, key)?.to_owned())
            .chain(args)
            .collect(),
    );
    spec.timeout = Duration::from_secs(seconds);
    process::execute(&spec)
}
pub fn install_barriers(config: &Value, record: &Value) -> Result<()> {
    require_root()?;
    let state = Path::new(string(config, "barrier_dir")?);
    owned_ancestors(state)?;
    provision_directory(state, 0o700)?;
    let retired = state.join("retired");
    if !array(config, "retired_units")?.is_empty() {
        provision_directory(&retired, 0o700)?;
    }
    durable::write_json(
        &state.join("start-policy.json"),
        &json!({"version":1,"generation":record["source_generation"],"authority_manifest":config["source_manifest"],"units":units(config)?}),
    )?;
    for unit in units(config)? {
        let selected = if array(config, "retired_units")?.contains(&json!(unit)) {
            retired.as_path()
        } else {
            state
        };
        let path = Path::new(string(config, "drop_in_root")?)
            .join(format!("{unit}.d/zzzz-harbor-db-backend-transition.conf"));
        owned_ancestors(&path)?;
        provision_directory(
            path.parent()
                .ok_or_else(|| invalid("missing drop-in parent"))?,
            0o755,
        )?;
        if fs::symlink_metadata(&path).is_ok() {
            check_transition_drop_in(config, &path, selected, &unit)?;
        } else {
            durable::atomic_write(&path, &transition_content(config, selected, &unit)?)?;
        }
    }
    let marker = state.join("inhibited.json");
    if marker.exists() && durable::read_json(&marker)?["intent"] != record["intent"] {
        return Err(invalid(
            "another transition owns the persistent startup barrier",
        ));
    }
    durable::write_json(
        &marker,
        &json!({"intent":record["intent"],"candidate":record["barrier_candidate"]}),
    )?;
    if !array(config, "retired_units")?.is_empty() {
        durable::write_json(
            &retired.join("inhibited.json"),
            &json!({"resource":config["resource"],"units":config["retired_units"]}),
        )?;
    }
    call(config, "systemctl", vec!["daemon-reload".into()], 30)?;
    inspect_barriers(config)
}
// systemctl renders paths using shell quoting. Parse just that output locally.
fn shell_words(s: &str) -> Result<Vec<String>> {
    let mut words = vec![];
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut active = false;
    for c in s.chars() {
        if escape {
            word.push(c);
            escape = false;
            active = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escape = true;
            active = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            active = true;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            active = true;
        } else if c.is_whitespace() {
            if active {
                words.push(std::mem::take(&mut word));
                active = false;
            }
        } else {
            word.push(c);
            active = true;
        }
    }
    if quote.is_some() || escape {
        return Err(invalid("invalid systemd DropInPaths"));
    }
    if active {
        words.push(word);
    }
    Ok(words)
}
pub fn inspect_barriers(config: &Value) -> Result<()> {
    let state = Path::new(string(config, "barrier_dir")?);
    owned(state, true)?;
    if !state.join("inhibited.json").exists() {
        return Err(invalid("transition startup barrier is absent"));
    }
    for unit in units(config)? {
        let retired = state.join("retired");
        let selected = if array(config, "retired_units")?.contains(&json!(unit)) {
            retired.as_path()
        } else {
            state
        };
        let path = Path::new(string(config, "drop_in_root")?)
            .join(format!("{unit}.d/zzzz-harbor-db-backend-transition.conf"));
        check_transition_drop_in(config, &path, selected, &unit)?;
        let output = call(
            config,
            "systemctl",
            vec![
                "show".into(),
                unit.clone(),
                "--property=DropInPaths".into(),
                "--value".into(),
            ],
            30,
        )?;
        if !shell_words(&process::text(&output)?)?.contains(&path.to_string_lossy().into_owned()) {
            return Err(invalid(
                "startup inhibition drop-in is not loaded by systemd",
            ));
        }
        let escaped: String = unit
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_string()
                } else {
                    format!("_{:02x}", c as u32)
                }
            })
            .collect();
        let observed = super::codec::decode(&call(
            config,
            "busctl",
            vec![
                "--json=short".into(),
                "get-property".into(),
                "org.freedesktop.systemd1".into(),
                format!("/org/freedesktop/systemd1/unit/{escaped}"),
                "org.freedesktop.systemd1.Unit".into(),
                "Conditions".into(),
            ],
            30,
        )?)?;
        let required = json!([
            "ConditionPathExists",
            false,
            true,
            selected.join("inhibited.json")
        ]);
        if observed["type"] != "a(sbbsi)"
            || !observed["data"].as_array().is_some_and(|a| {
                a.iter().any(|c| {
                    c.as_array()
                        .is_some_and(|a| a.len() >= 4 && a[..4] == required.as_array().unwrap()[..])
                })
            })
        {
            return Err(invalid(
                "systemd's effective startup inhibition condition is absent",
            ));
        }
    }
    Ok(())
}
pub fn startup_unit(state: &Path, unit: &str) -> Result<()> {
    require_root()?;
    owned_ancestors(state)?;
    owned(state, true)?;
    owned(&state.join("start-policy.json"), false)?;
    let policy = durable::read_json(&state.join("start-policy.json"))?;
    if policy["version"] != 1
        || !array(&policy, "units")?.contains(&json!(unit))
        || policy["generation"] != generation()?
    {
        return Err(invalid(
            "ordinary writer startup requires the explicitly released generation",
        ));
    }
    if state.join("inhibited.json").exists() {
        return Err(invalid(
            "ordinary writer startup is inhibited by a pending transition",
        ));
    }
    resource::check(&durable::read_config_json(Path::new(string(
        &policy,
        "authority_manifest",
    )?))?)?;
    Ok(())
}
pub fn release_barriers(config: &Value, record: &Value, aborted: bool) -> Result<()> {
    let state = Path::new(string(config, "barrier_dir")?);
    durable::write_json(
        &state.join("start-policy.json"),
        &json!({"version":1,"generation":record[if aborted{"source_generation"}else{"candidate"}],"authority_manifest":config[if aborted{"source_manifest"}else{"target_manifest"}],"units":if aborted{json!(units(config)?)}else{config["units"].clone()}}),
    )?;
    let mut paths = vec![state.join("inhibited.json")];
    if aborted && !array(config, "retired_units")?.is_empty() {
        paths.push(state.join("retired/inhibited.json"));
    }
    for path in paths {
        if path.exists() {
            owned(&path, false)?;
            let expected = if path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "retired")
            {
                json!({"resource":config["resource"],"units":config["retired_units"]})
            } else {
                json!({"intent":record["intent"],"candidate":record["barrier_candidate"]})
            };
            if durable::read_json(&path)? != expected {
                return Err(invalid("another transition owns the startup barrier"));
            }
            fs::remove_file(&path)?;
            durable::sync_directory(
                path.parent()
                    .ok_or_else(|| invalid("missing barrier parent"))?,
            )?;
        }
    }
    Ok(())
}
pub fn stop_units(config: &Value) -> Result<()> {
    let mut args = vec!["stop".into()];
    args.extend(units(config)?);
    call(
        config,
        "systemctl",
        args,
        config["timeout_seconds"]
            .as_u64()
            .ok_or_else(|| invalid("invalid timeout"))?,
    )?;
    for unit in units(config)? {
        let out = call(
            config,
            "systemctl",
            vec![
                "show".into(),
                unit,
                "--property=ActiveState".into(),
                "--value".into(),
            ],
            30,
        )?;
        if !matches!(process::text(&out)?.trim(), "inactive" | "failed") {
            return Err(invalid("an application writer has not stopped"));
        }
    }
    Ok(())
}
pub fn worker(
    config: &Value,
    command: &Value,
    substitutions: &BTreeMap<String, String>,
    leases: &[RawFd],
) -> Result<Value> {
    let account = process::account(string(command, "user")?)?;
    let uid = unsafe { libc::geteuid() };
    if uid != 0 && unsafe { libc::getuid() } != account.uid {
        return Err(invalid("transition worker identity differs"));
    }
    let argv = array(command, "argv")?
        .iter()
        .map(|v| {
            let s = v.as_str().ok_or_else(|| invalid("invalid worker argv"))?;
            Ok(substitutions
                .get(s)
                .cloned()
                .unwrap_or_else(|| s.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut spec = process::CommandSpec::new(argv);
    spec.timeout = Duration::from_secs(
        config["timeout_seconds"]
            .as_u64()
            .ok_or_else(|| invalid("invalid timeout"))?,
    );
    spec.leases = leases.to_vec();
    if uid == 0 {
        spec.identity = Some(account);
    }
    let mut environment: BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| matches!(k.as_str(), "PATH" | "HOME" | "LANG" | "TMPDIR"))
        .collect();
    environment.insert(
        "HARBOR_DB_LEASE_FDS".into(),
        leases
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
    );
    spec.environment = Some(environment);
    let data = process::execute(&spec)?;
    if data.iter().all(u8::is_ascii_whitespace) {
        Ok(Value::Null)
    } else {
        super::codec::decode(&data)
    }
}
pub struct Fence {
    pub leases: Vec<Lease>,
    pub fds: Vec<RawFd>,
}
pub fn fence(config: &Value, record: &mut Value, leases: &[RawFd]) -> Result<Fence> {
    let mut result = Fence {
        leases: vec![],
        fds: leases.to_vec(),
    };
    if config["postgres_manifest"].is_null() {
        return Ok(result);
    }
    let database = durable::read_config_json(Path::new(string(config, "postgres_manifest")?))?;
    let anchor = Path::new(string(&database, "state_dir")?).join("writer-fence.lock");
    let lease = durable::lock(&anchor, true, false)?;
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(lease.fd(), &mut info) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if info.st_uid != process::account("postgres")?.uid
        || info.st_mode & 0o022 != 0
        || info.st_mode & libc::S_IFMT != libc::S_IFREG
    {
        return Err(invalid(
            "PostgreSQL writer fence anchor is not service-owned",
        ));
    }
    result.fds.push(lease.fd());
    result.leases.push(lease);
    let observed = worker(
        config,
        &json!({"user":"postgres","argv":[Path::new(string(config,"storage_package")?).join("harbor-db-postgres"),"--config",config["postgres_manifest"],"inspect-fence","--token",record["writer_fence_token"],"--socket-dir",config["postgres_socket"],"--port",config["postgres_port"].to_string()]}),
        &BTreeMap::new(),
        &result.fds,
    )?;
    if observed["status"] != "ready" || observed["token"] != record["writer_fence_token"] {
        return Err(invalid("PostgreSQL live fence token differs"));
    }
    if !record["fence"].is_null() && record["fence"] != observed {
        return Err(invalid("PostgreSQL fence primary or HBA identity changed"));
    }
    record["fence"] = observed;
    let recovery = &database["recovery"];
    if recovery["require_writer_fence"] != true {
        return Err(invalid(
            "PostgreSQL transitions require token-bound whole-primary recovery acceptance",
        ));
    }
    for anchor in [
        Path::new(string(recovery, "backup_root")?).join("locks/mutate"),
        Path::new(string(recovery, "snapshot_file")?)
            .parent()
            .ok_or_else(|| invalid("missing snapshot parent"))?
            .join("recovery.lock"),
    ] {
        let lease = durable::lock(&anchor, true, false)?;
        result.fds.push(lease.fd());
        result.leases.push(lease);
    }
    Ok(result)
}
