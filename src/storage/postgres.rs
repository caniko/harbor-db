//! Explicit adoption, lifetime writer leases, and crash-resumable copy upgrades.
use super::{
    Result, codec, durable, invalid,
    pg_core::{self, major, path},
    process, string,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn recovery_admission(
    config: &Value,
    socket: Option<&Path>,
    port: Option<u16>,
) -> Result<Option<super::recovery::Admission>> {
    if config.get("recovery").is_none_or(Value::is_null) {
        Ok(None)
    } else {
        Ok(Some(super::recovery::evidence_admission(
            config,
            None,
            true,
            socket,
            port.unwrap_or(5432),
        )?))
    }
}

pub fn adopt(config: &Value, identifier: &str) -> Result<()> {
    pg_core::validate_config(config)?;
    pg_core::reject_upgrade(config)?;
    let state = path(config, "state_dir")?;
    let record = state.join("identity.json");
    let admission = recovery_admission(config, None, None)?;
    let lease = durable::lock(&state.join("lock"), false, !record.exists())?;
    let mut leases = admission.as_ref().map(|a| a.fds()).unwrap_or_default();
    leases.push(lease.fd());
    pg_core::reject_upgrade(config)?;
    let observed = pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        &leases,
    )?;
    if observed != identifier {
        return Err(invalid(
            "independently supplied system identifier does not match",
        ));
    }
    if record.exists() {
        pg_core::verify_identity_leased(config, &leases)?;
    } else {
        durable::write_json(&record, &pg_core::identity(config, &observed)?)?;
    }
    Ok(())
}

pub fn adopt_live(config: &Value, identifier: &str, socket: &Path, port: u16) -> Result<Value> {
    pg_core::validate_config(config)?;
    pg_core::reject_upgrade(config)?;
    let state = path(config, "state_dir")?;
    let record = state.join("identity.json");
    let adopted = record.exists();
    let admission = recovery_admission(config, Some(socket), Some(port))?;
    let lease = durable::lock(&state.join("lock"), adopted, !adopted)?;
    let mut leases = admission.as_ref().map(|a| a.fds()).unwrap_or_default();
    leases.push(lease.fd());
    pg_core::reject_upgrade(config)?;
    let changed = !(adopted || record.exists());
    if !changed {
        pg_core::verify_identity_leased(config, &leases)?;
    }
    let observed = pg_core::inspect_live_leased(config, identifier, socket, port, &leases)?;
    if changed {
        durable::write_json(&record, &pg_core::identity(config, identifier)?)?;
    }
    Ok(json!({"inspection":observed,"changed":changed}))
}

pub fn check(config: &Value) -> Result<()> {
    pg_core::validate_config(config)?;
    if !path(config, "state_dir")?.join("identity.json").exists() {
        return Err(invalid(
            "cluster is not adopted; explicit adoption is required",
        ));
    }
    let lease = durable::lock(&path(config, "state_dir")?.join("lock"), true, false)?;
    pg_core::reject_upgrade(config)?;
    pg_core::verify_identity_leased(config, &[lease.fd()])?;
    super::writer_fence::startup(config)?;
    Ok(())
}

pub fn serve(config: &Value) -> Result<()> {
    pg_core::validate_config(config)?;
    let lease = durable::lock(&path(config, "state_dir")?.join("lock"), true, false)?;
    pg_core::reject_upgrade(config)?;
    pg_core::verify_identity_leased(config, &[lease.fd()])?;
    let fence = super::writer_fence::startup_leased(config, &[lease.fd()])?;
    let mut command = Command::new(path(config, "package")?.join("bin/postgres"));
    command.args([
        "-D",
        string(config, "data_dir")?,
        "-c",
        &format!("data_directory={}", string(config, "data_dir")?),
        "-c",
        "fsync=on",
        "-c",
        "full_page_writes=on",
        "-c",
        "synchronous_commit=on",
    ]);
    if let Some(fence) = fence {
        command.args(["-c", &format!("hba_file={}", string(&fence, "hba_file")?)]);
    }
    // exec preserves the main PID and the shared flock's open file description.
    process::exec(&mut command, &[lease.fd()])
}

fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    Ok(path.with_file_name(format!(
        "{}{suffix}",
        path.file_name()
            .ok_or_else(|| invalid("cluster path has no name"))?
            .to_string_lossy()
    )))
}
fn strings(value: Option<&Value>) -> Result<Vec<String>> {
    match value {
        None => Ok(vec![]),
        Some(v) => v
            .as_array()
            .ok_or_else(|| invalid("command must be an argv array"))?
            .iter()
            .map(|s| {
                s.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("command argument must be a string"))
            })
            .collect(),
    }
}
fn worker(argv: Vec<String>, cwd: Option<&Path>, lease: &durable::Lease) -> Result<()> {
    let mut spec = pg_core::command(argv, false);
    spec.cwd = cwd.map(Path::to_path_buf);
    spec.leases.push(lease.fd());
    spec.timeout = Duration::from_secs(86400);
    process::execute(&spec)?;
    Ok(())
}

/// Copytree dereferences configuration links, matching Python shutil.copytree.
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let meta = fs::metadata(&from)?;
        if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else if meta.is_file() {
            fs::copy(&from, &to)?;
        } else {
            return Err(invalid("special storage cannot be copied for upgrade"));
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())?;
    Ok(())
}

const OLD_PORT: u16 = 55438;
const NEW_PORT: u16 = 55439;

/// The upgrade itself compares recovery record checks before and after pg_upgrade.
fn builtin_validation(settings: &Value) -> bool {
    settings.get("validate_records") == Some(&json!(true))
}

/// Run the configured record checks against a stopped cluster on a private
/// Unix socket (no TCP), then stop it again.
fn cluster_records(
    config: &Value,
    data: &Path,
    port: u16,
    state: &Path,
    lease: &durable::Lease,
) -> Result<Value> {
    let settings = config
        .get("recovery")
        .filter(|r| r["record_checks"].as_array().is_some_and(|c| !c.is_empty()))
        .ok_or_else(|| invalid("record validation requires recovery record checks"))?;
    let pg_ctl = path(config, "package")?
        .join("bin/pg_ctl")
        .display()
        .to_string();
    let quote = |p: &Path| super::postgres_drill::shell_quote(&p.display().to_string());
    let started = worker(
        vec![
            pg_ctl.clone(),
            "-D".into(),
            data.display().to_string(),
            "-l".into(),
            "/dev/null".into(),
            "-o".into(),
            format!(
                "-c listen_addresses='' -k {} -p {port} -c data_directory={}",
                quote(state),
                quote(data)
            ),
            "-w".into(),
            "start".into(),
        ],
        None,
        lease,
    );
    // A start that timed out may still be coming up: always try to stop it.
    let records = match started {
        Ok(()) => super::recovery::records(config, settings, state, port, &[lease.fd()]),
        Err(e) => Err(e),
    };
    let stopped = worker(
        vec![
            pg_ctl,
            "-D".into(),
            data.display().to_string(),
            "-m".into(),
            "fast".into(),
            "-w".into(),
            "stop".into(),
        ],
        None,
        lease,
    );
    let records = records?;
    stopped?;
    Ok(records)
}

/// Compare the staged cluster's record digests with the pre-upgrade snapshot.
fn validate_records(
    config: &Value,
    staging: &Path,
    state: &Path,
    lease: &durable::Lease,
) -> Result<()> {
    let snapshot = state.join("upgrade-records.json");
    let expected = durable::read_json(&snapshot)?;
    let actual = cluster_records(config, staging, NEW_PORT, state, lease)?;
    if actual != expected {
        let differing: Vec<_> = expected
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(name, digest)| actual.get(name.as_str()) != Some(digest))
            .map(|(name, _)| name.as_str())
            .collect();
        return Err(invalid(format!(
            "upgraded records differ from the pre-upgrade snapshot: {}",
            differing.join(", ")
        )));
    }
    fs::remove_file(snapshot)?;
    Ok(())
}

pub fn upgrade_contract(config: &Value) -> Result<Value> {
    let settings = config
        .get("upgrade")
        .ok_or_else(|| invalid("missing upgrade"))?;
    let mut tools = BTreeSet::new();
    for package in [path(config, "package")?, path(settings, "package")?] {
        // pg_upgrade invokes other package tools, including postgres, pg_dump
        // and pg_resetwal. Bind the complete executable directory as well.
        for entry in fs::read_dir(package.join("bin"))? {
            let tool = entry?.path();
            if tool.is_file() {
                tools.insert(tool);
            }
        }
        for binary in ["pg_ctl", "pg_controldata"] {
            tools.insert(package.join("bin").join(binary));
        }
    }
    for binary in ["initdb", "pg_upgrade"] {
        tools.insert(path(config, "package")?.join("bin").join(binary));
    }
    for key in ["copy_command", "validate_command"] {
        let argv = strings(settings.get(key))?;
        if key == "validate_command" && argv.is_empty() && !builtin_validation(settings) {
            return Err(invalid("upgrade requires a consumer validation command"));
        }
        if let Some(executable) = argv.first() {
            if settings["immutable_tools"] == true {
                let resolved =
                    durable::immutable_store_reference(Path::new(executable)).map_err(|error| {
                        invalid(format!(
                            "upgrade requires immutable store commands: {error}"
                        ))
                    })?;
                if fs::metadata(resolved)?.mode() & 0o111 == 0 {
                    return Err(invalid(
                        "upgrade command requires an executable immutable store file",
                    ));
                }
            }
            tools.insert(PathBuf::from(executable));
        }
        // Interpreter adapters can name additional executable/script files.
        tools.extend(
            argv.iter()
                .skip(1)
                .map(PathBuf::from)
                .filter(|p| p.is_file()),
        );
    }
    tools.extend(
        strings(settings.get("executable_files"))?
            .into_iter()
            .map(PathBuf::from),
    );
    let mut executables = serde_json::Map::new();
    for tool in tools {
        if !tool.is_absolute() || !tool.is_file() {
            return Err(invalid("upgrade executable or adapter file is absent"));
        }
        let resolved = if settings["immutable_tools"] == true {
            durable::immutable_store_reference(&tool).map_err(|error| {
                invalid(format!(
                    "upgrade requires immutable store tools and adapters: {error}"
                ))
            })?
        } else {
            fs::canonicalize(&tool)?
        };
        executables.insert(
            tool.to_str()
                .ok_or_else(|| invalid("invalid upgrade executable path"))?
                .into(),
            json!(codec::file_digest(&resolved)?),
        );
    }
    Ok(json!({"config":config,"executables":executables}))
}

/// Operator upgrades require immutable commands; synthetic library fixtures
/// retain their existing hash-bound mutable-tool contract.
pub fn validate_operator_upgrade_policy(config: &Value) -> Result<()> {
    let mut policy = config.clone();
    policy
        .get_mut("upgrade")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("upgrade policy must be an object"))?
        .insert("immutable_tools".into(), json!(true));
    upgrade_contract(&policy)?;
    Ok(())
}

pub fn upgrade(config: &Value, retry_incomplete: bool) -> Result<()> {
    pg_core::validate_config(config)?;
    let settings = config
        .get("upgrade")
        .ok_or_else(|| invalid("missing upgrade"))?;
    let mut source = config.clone();
    for key in ["data_dir", "major", "package"] {
        source[key] = settings
            .get(key)
            .ok_or_else(|| invalid(format!("missing upgrade {key}")))?
            .clone();
    }
    source["major"] = json!(major(&source)?);
    pg_core::validate_config(&source)?;
    if major(&source)?
        .parse::<u64>()
        .map_err(|_| invalid("invalid source major"))?
        >= major(config)?
            .parse::<u64>()
            .map_err(|_| invalid("invalid target major"))?
    {
        return Err(invalid("upgrade requires a newer PostgreSQL major"));
    }
    let state = path(config, "state_dir")?;
    let target = path(config, "data_dir")?;
    let staging = sibling(&target, ".harbor-staging")?;
    let source_copy = sibling(&target, ".harbor-source")?;
    let original = path(&source, "data_dir")?;
    let locations = [&original, &target, &staging, &source_copy];
    for (index, location) in locations.iter().enumerate() {
        if !pg_core::unredirected(location)? {
            return Err(invalid("upgrade storage paths must not be redirected"));
        }
        if locations[..index]
            .iter()
            .any(|other| location.starts_with(other) || other.starts_with(location))
        {
            return Err(invalid(
                "upgrade source, target, staging and source-copy paths must be disjoint",
            ));
        }
    }
    let journal_path = state.join("upgrade.json");
    let lease = durable::lock(&state.join("lock"), false, false)?;
    if !journal_path.exists() && state.join("identity.json").exists() {
        let current = durable::read_json(&state.join("identity.json"))?;
        if current.get("data_dir") == config.get("data_dir") {
            pg_core::verify_identity_leased(config, &[lease.fd()])?;
            return Ok(());
        }
    }
    pg_core::require_stopped_leased(&path(&source, "package")?, &original, &[lease.fd()])?;
    let journal = if journal_path.exists() {
        Some(durable::read_json(&journal_path)?)
    } else {
        None
    };
    let source_record = if let Some(journal) = &journal {
        let record = pg_core::identity(
            &source,
            &pg_core::inspect_cluster_leased(
                &path(&source, "package")?,
                &original,
                &major(&source)?,
                &[lease.fd()],
            )?,
        )?;
        let current = durable::read_json(&state.join("identity.json"))?;
        if current != record && Some(&current) != journal.get("identity") {
            return Err(invalid("registered identity changed during upgrade"));
        }
        record
    } else {
        pg_core::verify_identity_leased(&source, &[lease.fd()])?
    };
    let digest = pg_core::control_digest(&original)?;
    if fs::read_dir(original.join("pg_tblspc"))?
        .next()
        .transpose()?
        .is_some()
    {
        return Err(invalid(
            "external tablespaces are unsupported for staged upgrades",
        ));
    }
    if fs::symlink_metadata(original.join("pg_wal"))?
        .file_type()
        .is_symlink()
    {
        return Err(invalid(
            "external WAL storage is unsupported for staged upgrades",
        ));
    }
    let contract = upgrade_contract(config)?;
    let intent = json!({"version":2,"source":source_record,"source_control":digest,"target":{"resource":config["resource"],"data_dir":config["data_dir"],"major":config["major"],"package":config["package"]},"staging":staging,"source_copy":source_copy,"contract":contract});
    if let Some(journal) = journal {
        for (key, value) in intent
            .as_object()
            .ok_or_else(|| invalid("invalid upgrade intent"))?
        {
            if journal.get(key) != Some(value) {
                return Err(invalid("upgrade source or contract changed; cannot resume"));
            }
        }
        if journal["phase"] == "ready" {
            return publish_leased(config, &journal, &[lease.fd()]);
        }
        if journal["phase"] != "building" || !retry_incomplete {
            return Err(invalid(
                "incomplete upgrade; use --retry-incomplete after inspection",
            ));
        }
        if staging.exists() {
            let abandoned = sibling(&staging, ".interrupted")?;
            if abandoned.exists() {
                return Err(invalid(format!(
                    "inspect preserved incomplete cluster: {}",
                    abandoned.display()
                )));
            }
            pg_core::require_stopped_leased(&path(config, "package")?, &staging, &[lease.fd()])?;
            fs::rename(&staging, abandoned)?;
            durable::sync_directory(
                staging
                    .parent()
                    .ok_or_else(|| invalid("missing staging parent"))?,
            )?;
        }
        if source_copy.exists() {
            let abandoned = sibling(&source_copy, ".interrupted")?;
            if abandoned.exists() || source_copy.join("postmaster.pid").exists() {
                return Err(invalid(format!(
                    "inspect preserved source copy: {}",
                    source_copy.display()
                )));
            }
            fs::rename(&source_copy, abandoned)?;
            durable::sync_directory(
                source_copy
                    .parent()
                    .ok_or_else(|| invalid("missing source-copy parent"))?,
            )?;
        }
    }
    if target.exists()
        && !fs::symlink_metadata(&target)?.file_type().is_symlink()
        && fs::read_dir(&target)?.next().transpose()?.is_none()
    {
        fs::remove_dir(&target)?;
        durable::sync_directory(
            target
                .parent()
                .ok_or_else(|| invalid("missing target parent"))?,
        )?;
    }
    if target.exists() || staging.exists() || source_copy.exists() {
        return Err(invalid(
            "unregistered destination exists; explicit inspection required",
        ));
    }
    let mut journal = intent;
    journal["phase"] = json!("building");
    durable::write_json(&journal_path, &journal)?;
    let mut copy = strings(settings.get("copy_command"))?;
    if copy.is_empty() {
        copy_tree(&original, &source_copy)?;
    } else {
        copy.extend([
            original.display().to_string(),
            source_copy.display().to_string(),
        ]);
        worker(copy, None, &lease)?;
    }
    if builtin_validation(settings) {
        let records = cluster_records(&source, &source_copy, OLD_PORT, &state, &lease)?;
        durable::write_json(&state.join("upgrade-records.json"), &records)?;
    }
    let package = path(config, "package")?;
    let mut init = vec![
        package.join("bin/initdb").display().to_string(),
        "-D".into(),
        staging.display().to_string(),
    ];
    init.extend(strings(settings.get("initdb_args"))?);
    worker(init, None, &lease)?;
    let extra = match settings.get("extra_config") {
        Some(v) => v.as_str().ok_or_else(|| invalid("invalid extra_config"))?,
        None => "",
    };
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(staging.join("postgresql.conf"))?;
    writeln!(file, "\n{extra}")?;
    drop(file);
    worker(
        vec![
            package.join("bin/pg_upgrade").display().to_string(),
            format!("--old-bindir={}/bin", path(&source, "package")?.display()),
            format!("--new-bindir={}/bin", package.display()),
            format!("--old-datadir={}", source_copy.display()),
            format!("--new-datadir={}", staging.display()),
            format!("--socketdir={}", state.display()),
            "--old-port=55438".into(),
            "--new-port=55439".into(),
            format!(
                "--old-options=-c listen_addresses='' -c data_directory={}",
                super::postgres_drill::shell_quote(&source_copy.display().to_string())
            ),
            format!(
                "--new-options=-c listen_addresses='' -c data_directory={}",
                super::postgres_drill::shell_quote(&staging.display().to_string())
            ),
        ],
        Some(&staging),
        &lease,
    )?;
    pg_core::require_stopped_leased(&package, &staging, &[lease.fd()])?;
    let mut validator = strings(settings.get("validate_command"))?;
    if validator.is_empty() && !builtin_validation(settings) {
        return Err(invalid("upgrade requires a consumer validation command"));
    }
    if !validator.is_empty() {
        validator.push(staging.display().to_string());
        worker(validator, None, &lease)?;
    }
    if builtin_validation(settings) {
        validate_records(config, &staging, &state, &lease)?;
    }
    pg_core::require_stopped_leased(&package, &staging, &[lease.fd()])?;
    if pg_core::control_digest(&original)? != digest {
        return Err(invalid("registered source changed during offline upgrade"));
    }
    if upgrade_contract(config)? != contract {
        return Err(invalid(
            "upgrade executable or contract changed during validation",
        ));
    }
    journal["identity"] = pg_core::identity(
        config,
        &pg_core::inspect_cluster_leased(&package, &staging, &major(config)?, &[lease.fd()])?,
    )?;
    durable::sync_tree(&staging)?;
    journal["phase"] = json!("ready");
    durable::write_json(&journal_path, &journal)?;
    publish_leased(config, &journal, &[lease.fd()])
}

pub fn publish(config: &Value, journal: &Value) -> Result<()> {
    publish_leased(config, journal, &[])
}

fn publish_leased(config: &Value, journal: &Value, leases: &[std::os::fd::RawFd]) -> Result<()> {
    let target = path(config, "data_dir")?;
    let staging = path(journal, "staging")?;
    if target.exists() && staging.exists() {
        return Err(invalid(
            "ambiguous publication: both target and staging exist",
        ));
    }
    let location = if staging.exists() { &staging } else { &target };
    pg_core::require_stopped_leased(&path(config, "package")?, location, leases)?;
    let observed = pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        location,
        &major(config)?,
        leases,
    )?;
    if Some(&pg_core::identity(config, &observed)?) != journal.get("identity") {
        return Err(invalid("validated upgrade identity mismatch"));
    }
    if staging.exists() {
        fs::rename(&staging, &target)?;
    }
    durable::sync_directory(
        target
            .parent()
            .ok_or_else(|| invalid("missing target parent"))?,
    )?;
    let state = path(config, "state_dir")?;
    durable::write_json(
        &state.join("previous-identity.json"),
        journal
            .get("source")
            .ok_or_else(|| invalid("missing upgrade source"))?,
    )?;
    durable::write_json(
        &state.join("identity.json"),
        journal
            .get("identity")
            .ok_or_else(|| invalid("missing upgrade identity"))?,
    )?;
    fs::remove_file(state.join("upgrade.json"))?;
    durable::sync_directory(&state)
}
