//! Explicit adoption, lifetime writer leases, and crash-resumable copy upgrades.
use super::{
    Result, durable, invalid,
    pg_core::{self, major, path},
    process, string,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
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
        Ok(Some(super::recovery::admission_lease(
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
    if [&target, &staging, &source_copy].contains(&&original) {
        return Err(invalid("source and destination must differ"));
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
    let intent = json!({"version":1,"source":source_record,"source_control":digest,"target":{"resource":config["resource"],"data_dir":config["data_dir"],"major":config["major"],"package":config["package"]},"staging":staging,"source_copy":source_copy});
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
    if validator.is_empty() {
        return Err(invalid("upgrade requires a consumer validation command"));
    }
    validator.push(staging.display().to_string());
    worker(validator, None, &lease)?;
    pg_core::require_stopped_leased(&package, &staging, &[lease.fd()])?;
    if pg_core::control_digest(&original)? != digest {
        return Err(invalid("registered source changed during offline upgrade"));
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
