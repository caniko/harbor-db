//! Read-only recovery admission and record-level executed acceptance evidence.
//! Source-local evidence is additionally bound to its selected immutable capture.
use super::{Result, codec, durable, invalid, pg_core, process, string, writer_fence};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::{fd::RawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use writer_fence::{exists, major, path};

fn clock(now: Option<i64>) -> Result<i64> {
    Ok(match now {
        Some(value) => value,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("invalid system clock"))?
            .as_secs()
            .try_into()
            .map_err(|_| invalid("invalid system clock"))?,
    })
}
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid("recovery path must be absolute and not redirected"));
    }
    // canonicalize existing prefixes too: output artifacts need not exist yet.
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !exists(ancestor) {
        missing.push(
            ancestor
                .file_name()
                .ok_or_else(|| invalid("invalid recovery path"))?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| invalid("invalid recovery path"))?;
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    if resolved.as_os_str() != path.as_os_str() {
        return Err(invalid(format!(
            "recovery path must be absolute and not redirected: {}",
            path.display()
        )));
    }
    Ok(path.to_owned())
}
fn digest(path: &Path) -> Result<String> {
    codec::file_digest(&absolute(path)?)
}
pub fn fresh(timestamp: &Value, now: i64, max_age: i64) -> Result<()> {
    let timestamp = timestamp
        .as_i64()
        .ok_or_else(|| invalid("recovery evidence timestamp is stale, future or invalid"))?;
    if !now
        .checked_sub(timestamp)
        .is_some_and(|age| age >= 0 && age <= max_age)
    {
        return Err(invalid(
            "recovery evidence timestamp is stale, future or invalid",
        ));
    }
    Ok(())
}
pub fn policy(config: &Value) -> Result<&Value> {
    let settings = config
        .get("recovery")
        .filter(|v| v.is_object())
        .ok_or_else(|| invalid("missing recovery policy"))?;
    super::recovery_repository::source_local(settings)?;
    if settings
        .get("require_writer_fence")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(invalid(
            "recovery writer fence requirement must be a boolean",
        ));
    }
    let snapshot = string(settings, "snapshot_file")?;
    if snapshot == string(settings, "receipt_file")?
        || settings
            .get("off_host_receipt_file")
            .and_then(Value::as_str)
            == Some(snapshot)
    {
        return Err(invalid(
            "recovery receipts must not overwrite the record snapshot",
        ));
    }
    if !writer_fence::valid_identifier(string(settings, "system_identifier")?) {
        return Err(invalid(
            "recovery requires an independently recorded system identifier",
        ));
    }
    if settings["max_age_seconds"].as_i64().is_none_or(|n| n <= 0) {
        return Err(invalid("recovery evidence age must be positive"));
    }
    let checks = settings["record_checks"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("recovery requires uniquely named record checks"))?;
    let mut names = BTreeSet::new();
    for item in checks {
        for key in ["name", "database", "sql"] {
            let text = string(item, key)?;
            if text.trim().is_empty() || text.contains('\0') {
                return Err(invalid("invalid recovery record check"));
            }
        }
        if !names.insert(string(item, "name")?) {
            return Err(invalid("recovery requires uniquely named record checks"));
        }
    }
    Ok(settings)
}
pub fn contract(settings: &Value) -> Result<String> {
    Ok(codec::digest(&codec::encode(
        &settings["record_checks"],
        false,
    )?))
}
pub fn lsn(value: &Value) -> Result<u64> {
    let value = value
        .as_str()
        .ok_or_else(|| invalid("invalid recovery LSN"))?;
    let (high, low) = value
        .split_once('/')
        .ok_or_else(|| invalid("invalid recovery LSN"))?;
    let parse = |part: &str| -> Result<u64> {
        if part.is_empty() || part.len() > 8 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid recovery LSN"));
        }
        u64::from_str_radix(part, 16).map_err(|_| invalid("invalid recovery LSN"))
    };
    Ok((parse(high)? << 32) + parse(low)?)
}
fn age(settings: &Value) -> Result<i64> {
    settings["max_age_seconds"]
        .as_i64()
        .ok_or_else(|| invalid("invalid evidence age"))
}
fn backup(
    config: &Value,
    settings: &Value,
    now: i64,
    leases: &[RawFd],
) -> Result<(PathBuf, Value)> {
    let (directory, binding, _) = selected_backup(config, settings, now, leases)?;
    Ok((directory, binding))
}
fn selected_backup(
    config: &Value,
    settings: &Value,
    now: i64,
    leases: &[RawFd],
) -> Result<(
    PathBuf,
    Value,
    Option<super::recovery_repository::SelectedCapture>,
)> {
    if super::recovery_repository::source_local(settings)? {
        let selected = super::recovery_repository::select(config, settings, now)?;
        if pg_core::inspect_cluster_leased(
            &path(config, "package")?,
            selected.directory(),
            &major(config)?,
            leases,
        )? != string(settings, "system_identifier")?
        {
            return Err(invalid(
                "backup control-file identity differs from the primary",
            ));
        }
        return Ok((
            selected.directory().to_owned(),
            selected.binding().clone(),
            Some(selected),
        ));
    }
    let root = absolute(&path(settings, "backup_root")?)?;
    let marker = absolute(&root.join("LAST_SUCCESS"))?;
    let mut content = String::new();
    durable::open_regular(&marker, false)?
        .take(256)
        .read_to_string(&mut content)?;
    let identifier = content.trim();
    if identifier.is_empty()
        || identifier.len() > 128
        || !identifier.as_bytes()[0].is_ascii_alphanumeric()
        || !identifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        || identifier.ends_with(".partial")
    {
        return Err(invalid("invalid completed backup identifier"));
    }
    let directory = absolute(&root.join("base").join(identifier))?;
    let manifest = absolute(&directory.join("backup_manifest"))?;
    let metadata = absolute(&root.join("base").join(format!("{identifier}.meta.json")))?;
    let meta = durable::read_json(&metadata)?;
    if meta["backup_id"] != identifier
        || major(&json!({"major":meta["pg_major"]}))? != major(config)?
        || meta["system_identifier"] != settings["system_identifier"]
    {
        return Err(invalid(
            "backup identity does not match the declared primary",
        ));
    }
    if lsn(&meta["post_backup_lsn"])? <= lsn(&meta["backup_stop_lsn"])? {
        return Err(invalid("backup recovery point must follow its stop LSN"));
    }
    fresh(
        &json!(fs::metadata(&manifest)?.mtime()),
        now,
        age(settings)?,
    )?;
    if pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        &directory,
        &major(config)?,
        leases,
    )? != string(settings, "system_identifier")?
    {
        return Err(invalid(
            "backup control-file identity differs from the primary",
        ));
    }
    if meta.get("epoch_id").is_none() {
        return Err(invalid("missing backup epoch"));
    }
    Ok((
        directory,
        json!({"backup_id":identifier,"system_identifier":settings["system_identifier"],"major":major(config)?,"epoch_id":meta["epoch_id"],"manifest_sha256":digest(&manifest)?,"recovery_target_lsn":meta["post_backup_lsn"],"metadata_sha256":digest(&metadata)?}),
        None,
    ))
}
fn verify_backup(config: &Value, directory: &Path, leases: &[RawFd]) -> Result<()> {
    let seconds = config["recovery"]["verify_timeout_seconds"]
        .as_f64()
        .filter(|v| *v > 0.0)
        .ok_or_else(|| invalid("invalid backup verification timeout"))?;
    let timeout = Duration::try_from_secs_f64(seconds)
        .map_err(|_| invalid("invalid backup verification timeout"))?;
    let mut spec = process::CommandSpec::new(vec![
        path(config, "package")?
            .join("bin/pg_verifybackup")
            .to_string_lossy()
            .into_owned(),
        "--no-parse-wal".into(),
        directory.to_string_lossy().into_owned(),
    ]);
    spec.timeout = timeout;
    spec.leases = leases.to_vec();
    process::execute(&spec)?;
    Ok(())
}
pub fn query(
    config: &Value,
    socket: &Path,
    port: u16,
    database: &str,
    sql: &str,
) -> Result<String> {
    query_leased(config, socket, port, database, sql, &[])
}
pub fn query_leased(
    config: &Value,
    socket: &Path,
    port: u16,
    database: &str,
    sql: &str,
    leases: &[RawFd],
) -> Result<String> {
    if !socket.is_absolute() || socket.to_string_lossy().contains(',') || port == 0 {
        return Err(invalid(
            "recovery queries require a local Unix socket and valid port",
        ));
    }
    let mut env = std::env::vars()
        .filter(|(k, _)| !k.starts_with("PG"))
        .collect::<BTreeMap<_, _>>();
    env.insert("PGCONNECT_TIMEOUT".into(), "5".into());
    let mut spec = process::CommandSpec::new(vec![
        path(config, "package")?
            .join("bin/psql")
            .to_string_lossy()
            .into_owned(),
        "--no-psqlrc".into(),
        "--no-password".into(),
        "--quiet".into(),
        format!("--host={}", socket.display()),
        format!("--port={port}"),
        "--username=postgres".into(),
        format!("--dbname={database}"),
        "--set=ON_ERROR_STOP=1".into(),
        "--tuples-only".into(),
        "--no-align".into(),
        "--command".into(),
        format!(
            "BEGIN READ ONLY; SET LOCAL TimeZone = 'UTC'; SET LOCAL DateStyle = 'ISO,YMD'; SET LOCAL bytea_output = 'hex';\n{sql}\n;COMMIT;"
        ),
    ]);
    spec.environment = Some(env);
    spec.leases = leases.to_vec();
    process::text(&process::execute(&spec)?)
}
fn records(
    config: &Value,
    settings: &Value,
    socket: &Path,
    port: u16,
    leases: &[RawFd],
) -> Result<Value> {
    let mut result = serde_json::Map::new();
    for item in settings["record_checks"]
        .as_array()
        .ok_or_else(|| invalid("invalid record checks"))?
    {
        result.insert(
            string(item, "name")?.into(),
            json!(codec::digest(
                query_leased(
                    config,
                    socket,
                    port,
                    string(item, "database")?,
                    string(item, "sql")?,
                    leases
                )?
                .as_bytes()
            )),
        );
    }
    Ok(Value::Object(result))
}
fn evidence(
    path: &Path,
    label: &str,
    binding: &Value,
    settings: &Value,
    now: i64,
) -> Result<Value> {
    let value = durable::read_json(&absolute(path)?)?;
    if value["version"] != 1
        || binding
            .as_object()
            .ok_or_else(|| invalid("invalid backup binding"))?
            .iter()
            .any(|(key, expected)| value.get(key) != Some(expected))
    {
        return Err(invalid(format!("{label} is for a different backup")));
    }
    if value["record_contract_sha256"] != contract(settings)? {
        return Err(invalid(format!("{label} record-check contract differs")));
    }
    fresh(&value["completed_at"], now, age(settings)?)?;
    Ok(value)
}
fn evidence_lease(settings: &Value, inspect: bool) -> Result<durable::Lease> {
    let snapshot = absolute(&path(settings, "snapshot_file")?)?;
    let anchor = snapshot
        .parent()
        .ok_or_else(|| invalid("invalid snapshot parent"))?
        .join("recovery.lock");
    if inspect && !exists(&anchor) {
        return Err(invalid(
            "missing record snapshot lease; execute record snapshot and restore certification",
        ));
    }
    durable::lock(&anchor, inspect, !inspect)
}
fn backup_lease(settings: &Value) -> Result<durable::Lease> {
    durable::lock(
        &absolute(&path(settings, "backup_root")?)?.join("locks/mutate"),
        true,
        false,
    )
}
fn backup_evidence_leases(
    settings: &Value,
    inspect: bool,
) -> Result<(durable::Lease, durable::Lease)> {
    let backup = backup_lease(settings)?;
    let evidence = evidence_lease(settings, inspect)?;
    Ok((backup, evidence))
}
/// Resolve source evidence while the caller retains its repository lease.
/// The configured path continues to determine legacy evidence and lock anchors.
pub fn source_snapshot_path(config: &Value, settings: &Value) -> Result<PathBuf> {
    if super::recovery_repository::source_local(settings)? {
        Ok(
            super::recovery_repository::select(config, settings, clock(None)?)?
                .snapshot_path()
                .to_owned(),
        )
    } else {
        path(settings, "snapshot_file")
    }
}
fn snapshot_evidence(config: &Value, settings: &Value, binding: &Value, now: i64) -> Result<Value> {
    let source = evidence(
        &source_snapshot_path(config, settings)?,
        "record snapshot",
        binding,
        settings,
        now,
    )?;
    validate_capture_source(config, settings, &source, now)?;
    Ok(source)
}
fn validate_capture_source(
    config: &Value,
    settings: &Value,
    source: &Value,
    now: i64,
) -> Result<()> {
    // Every caller retains the shared repository lease, including certifiers
    // without a local primary fence. The source token belongs to the capture.
    if super::recovery_repository::source_local(settings)? {
        let selected = super::recovery_repository::select(config, settings, now)?;
        if selected
            .binding()
            .as_object()
            .unwrap()
            .iter()
            .any(|(key, value)| source.get(key) != Some(value))
        {
            return Err(invalid("record snapshot is for a different backup"));
        }
        selected.validate_snapshot(source["writer_fence_token"].as_str(), &source["records"])?;
        if source["completed_at"] != selected.metadata()["completed_at"] {
            return Err(invalid(
                "source-local snapshot completion differs from its immutable capture",
            ));
        }
    }
    Ok(())
}
pub struct WriterExclusion {
    pub binding: Option<Value>,
    pub lease: Option<durable::Lease>,
}
pub fn writer_exclusion(
    config: &Value,
    socket: Option<&Path>,
    port: u16,
) -> Result<WriterExclusion> {
    writer_exclusion_leased(config, socket, port, &[])
}
fn writer_exclusion_leased(
    config: &Value,
    socket: Option<&Path>,
    port: u16,
    outer: &[RawFd],
) -> Result<WriterExclusion> {
    let settings = policy(config)?;
    let active = config.get("state_dir").is_some() && exists(&writer_fence::marker(config)?);
    if settings["require_writer_fence"] != true && !active {
        return Ok(WriterExclusion {
            binding: None,
            lease: None,
        });
    }
    pg_core::validate_config(config)?;
    let anchor = path(config, "state_dir")?.join("writer-fence.lock");
    if !exists(&anchor) {
        return Err(invalid(
            "required writer fence is absent; acquire it before recovery",
        ));
    }
    let lease = durable::lock(&anchor, true, false)?;
    let mut leases = outer.to_vec();
    leases.push(lease.fd());
    let record = writer_fence::startup_leased(config, &leases)?
        .ok_or_else(|| invalid("required writer fence does not bind the declared primary"))?;
    if record["system_identifier"] != settings["system_identifier"] {
        return Err(invalid(
            "required writer fence does not bind the declared primary",
        ));
    }
    if let Some(socket) = socket {
        writer_fence::inspect_live_leased(
            config,
            string(&record, "token")?,
            socket,
            port,
            &leases,
        )?;
    }
    Ok(WriterExclusion {
        binding: Some(
            json!({"token":record["token"],"system_identifier":record["system_identifier"],"hba_sha256":record["hba_sha256"]}),
        ),
        lease: Some(lease),
    })
}
fn source_fence<'a>(settings: &Value, source: &'a Value) -> Result<Option<&'a str>> {
    let binding = source.get("writer_fence_token").filter(|v| !v.is_null());
    if binding.is_some_and(|v| !v.as_str().is_some_and(writer_fence::valid_token)) {
        return Err(invalid("record snapshot writer fence binding is invalid"));
    }
    if settings["require_writer_fence"] == true && binding.is_none() {
        return Err(invalid(
            "record snapshot has no required writer fence binding",
        ));
    }
    Ok(binding.and_then(Value::as_str))
}
fn fds(fence: &WriterExclusion, backup: &durable::Lease, evidence: &durable::Lease) -> Vec<RawFd> {
    let mut result = vec![backup.fd(), evidence.fd()];
    if let Some(lease) = &fence.lease {
        result.push(lease.fd());
    }
    result
}
fn result(binding: &Value) -> Result<Value> {
    let mut result = binding.clone();
    let map = result
        .as_object_mut()
        .ok_or_else(|| invalid("invalid backup binding"))?;
    map.insert("version".into(), json!(1));
    Ok(result)
}
pub fn snapshot(config: &Value, socket: &Path, port: u16, now: Option<i64>) -> Result<Value> {
    snapshot_leased(config, socket, port, now, &[])
}
/// Outer action leases are borrowed and inherited by every nested worker.
pub fn snapshot_leased(
    config: &Value,
    socket: &Path,
    port: u16,
    now: Option<i64>,
    outer: &[RawFd],
) -> Result<Value> {
    let settings = policy(config)?;
    let captured = super::recovery_repository::source_local(settings)?;
    if captured && settings["require_writer_fence"] != true {
        return Err(invalid("source-local snapshots require the writer fence"));
    }
    let now = clock(now)?;
    let fence = writer_exclusion_leased(config, Some(socket), port, outer)?;
    let (backup_lease, evidence_lease) = backup_evidence_leases(settings, false)?;
    let mut leases = fds(&fence, &backup_lease, &evidence_lease);
    leases.extend_from_slice(outer);
    let (directory, binding, selected) = selected_backup(config, settings, now, &leases)?;
    pg_core::inspect_live_leased(
        config,
        string(settings, "system_identifier")?,
        socket,
        port,
        &leases,
    )?;
    verify_backup(config, &directory, &leases)?;
    let mut result = result(&binding)?;
    result["completed_at"] = json!(now);
    result["record_contract_sha256"] = json!(contract(settings)?);
    result["records"] = records(config, settings, socket, port, &leases)?;
    if let Some(binding) = &fence.binding {
        result["writer_fence_token"] = binding["token"].clone();
    }
    if let Some(selected) = selected {
        selected.validate_snapshot(result["writer_fence_token"].as_str(), &result["records"])?;
        return snapshot_evidence(config, settings, &binding, now);
    }
    durable::write_json(&absolute(&path(settings, "snapshot_file")?)?, &result)?;
    Ok(result)
}
fn hostname() -> Result<String> {
    let mut buffer = [0u8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let end = buffer
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| invalid("hostname exceeds limit"))?;
    String::from_utf8(buffer[..end].to_vec()).map_err(|_| invalid("hostname is not UTF-8"))
}
pub fn certify(
    config: &Value,
    data: &Path,
    socket: &Path,
    port: u16,
    now: Option<i64>,
    host: Option<&str>,
) -> Result<Value> {
    let settings = policy(config)?;
    let now = clock(now)?;
    let restored = absolute(data)?;
    if restored == absolute(&path(config, "data_dir")?)? {
        return Err(invalid(
            "cannot certify the authoritative primary as a restored copy",
        ));
    }
    let (backup_lease, evidence_lease) = backup_evidence_leases(settings, false)?;
    let leases = [backup_lease.fd(), evidence_lease.fd()];
    let (directory, binding) = backup(config, settings, now, &leases)?;
    let source = snapshot_evidence(config, settings, &binding, now)?;
    source_fence(settings, &source)?;
    verify_backup(config, &directory, &leases)?;
    let observed: Value = serde_json::from_str(&query_leased(
        config,
        socket,
        port,
        "postgres",
        "SELECT json_build_object('data_dir', current_setting('data_directory'), 'major', (current_setting('server_version_num')::int / 10000)::text, 'system_identifier', system_identifier::text, 'read_only', current_setting('default_transaction_read_only'), 'in_recovery', pg_is_in_recovery(), 'replay_lsn', pg_last_wal_replay_lsn()::text) FROM pg_control_system()",
        &leases,
    )?)?;
    let expected = json!({"data_dir":restored,"major":major(config)?,"system_identifier":settings["system_identifier"],"read_only":"on","in_recovery":false});
    if expected
        .as_object()
        .unwrap()
        .iter()
        .any(|(key, v)| observed.get(key) != Some(v))
    {
        return Err(invalid(
            "restored endpoint is not the declared disposable read-only recovery",
        ));
    }
    if lsn(&observed["replay_lsn"])? < lsn(&binding["recovery_target_lsn"])? {
        return Err(invalid(
            "restored endpoint did not reach the recovery point",
        ));
    }
    let actual = records(config, settings, socket, port, &leases)?;
    if actual != source["records"] {
        return Err(invalid(
            "restored application records differ from the source snapshot",
        ));
    }
    let mut result = result(&binding)?;
    result["status"] = json!("ready");
    result["completed_at"] = json!(now);
    result["record_contract_sha256"] = json!(contract(settings)?);
    result["records"] = actual;
    result["snapshot_sha256"] = json!(digest(&source_snapshot_path(config, settings)?)?);
    result["executor_host"] = json!(match host {
        Some(host) => host.to_owned(),
        None => hostname()?,
    });
    result["restored_data_dir"] = json!(restored);
    result["replay_lsn"] = observed["replay_lsn"].clone();
    durable::write_json(&absolute(&path(settings, "receipt_file")?)?, &result)?;
    Ok(result)
}
pub fn validate_receipt(
    config: &Value,
    settings: &Value,
    source: &Value,
    binding: &Value,
    receipt: &Value,
    label: &str,
) -> Result<()> {
    let completed = receipt["completed_at"]
        .as_i64()
        .ok_or_else(|| invalid("invalid receipt timestamp"))?;
    let source_completed = source["completed_at"]
        .as_i64()
        .ok_or_else(|| invalid("invalid snapshot timestamp"))?;
    if receipt["status"] != "ready"
        || receipt.get("records") != source.get("records")
        || receipt["snapshot_sha256"] != digest(&source_snapshot_path(config, settings)?)?
        || receipt["restored_data_dir"] == config["data_dir"]
        || receipt["restored_data_dir"]
            .as_str()
            .is_none_or(str::is_empty)
        || completed < source_completed
        || lsn(&receipt["replay_lsn"])? < lsn(&binding["recovery_target_lsn"])?
    {
        return Err(invalid(format!(
            "{label} is not complete record-level recovery evidence"
        )));
    }
    if label.starts_with("off-host")
        && (receipt["executor_host"].as_str().is_none_or(str::is_empty)
            || receipt["executor_host"] == settings["source_hostname"])
    {
        return Err(invalid(
            "off-host recovery must execute on an independent host",
        ));
    }
    Ok(())
}
fn check_evidence(
    config: &Value,
    settings: &Value,
    backup: (&Path, &Value),
    now: i64,
    verify_contents: bool,
    fence: Option<&Value>,
    leases: &[RawFd],
) -> Result<Value> {
    let (directory, binding) = backup;
    let source = snapshot_evidence(config, settings, binding, now)?;
    let recorded = source_fence(settings, &source)?;
    if settings["require_writer_fence"] == true
        && (fence.is_none() || recorded != fence.and_then(|v| v["token"].as_str()))
    {
        return Err(invalid(
            "record snapshot writer fence differs from the retained window",
        ));
    }
    let expected = settings["record_checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| string(v, "name"))
        .collect::<Result<BTreeSet<_>>>()?;
    let actual = source["records"]
        .as_object()
        .ok_or_else(|| invalid("record snapshot is incomplete"))?;
    if actual.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected
        || actual.values().any(|v| {
            !v.as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
    {
        return Err(invalid("record snapshot is incomplete"));
    }
    if verify_contents {
        verify_backup(config, directory, leases)?;
    }
    let mut result = binding.clone();
    result["status"] = json!(if verify_contents {
        "ready"
    } else {
        "preflight-ready"
    });
    result["off_host"] = Value::Null;
    let mut paths = vec![(path(settings, "receipt_file")?, "restore acceptance")];
    if let Some(remote) = settings["off_host_receipt_file"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        paths.push((PathBuf::from(remote), "off-host restore acceptance"));
    }
    for (path, label) in paths {
        let receipt = evidence(&path, label, binding, settings, now)?;
        validate_receipt(config, settings, &source, binding, &receipt, label)?;
        if label.starts_with("off-host") {
            result["off_host"] = receipt["executor_host"].clone();
        }
    }
    Ok(result)
}
/// Retains every acceptance lease until authority publication has completed.
pub struct Admission {
    pub result: Value,
    pub fence: WriterExclusion,
    pub backup_lease: durable::Lease,
    pub evidence_lease: durable::Lease,
}
impl Admission {
    pub fn fds(&self) -> Vec<RawFd> {
        fds(&self.fence, &self.backup_lease, &self.evidence_lease)
    }
}
pub fn admission_lease(
    config: &Value,
    now: Option<i64>,
    verify_contents: bool,
    socket: Option<&Path>,
    port: u16,
) -> Result<Admission> {
    admission_with_outer(config, now, verify_contents, socket, port, &[])
}
fn admission_with_outer(
    config: &Value,
    now: Option<i64>,
    verify_contents: bool,
    socket: Option<&Path>,
    port: u16,
    outer: &[RawFd],
) -> Result<Admission> {
    let settings = policy(config)?;
    let now = clock(now)?;
    let fence = writer_exclusion_leased(config, socket, port, outer)?;
    let backup_lease = backup_lease(settings)?;
    let mut leases = outer.to_vec();
    leases.push(backup_lease.fd());
    if let Some(lease) = &fence.lease {
        leases.push(lease.fd());
    }
    let (directory, binding) = backup(config, settings, now, &leases)?;
    let evidence_lease = evidence_lease(settings, true)?;
    leases.push(evidence_lease.fd());
    let result = check_evidence(
        config,
        settings,
        (&directory, &binding),
        now,
        verify_contents,
        fence.binding.as_ref(),
        &leases,
    )?;
    Ok(Admission {
        result,
        fence,
        backup_lease,
        evidence_lease,
    })
}
pub fn admission(
    config: &Value,
    now: Option<i64>,
    verify_contents: bool,
    socket: Option<&Path>,
    port: u16,
) -> Result<Value> {
    Ok(admission_lease(config, now, verify_contents, socket, port)?.result)
}
pub fn check(config: &Value, now: Option<i64>) -> Result<Value> {
    admission(config, now, true, None, 5432)
}
pub fn preflight(config: &Value, now: Option<i64>) -> Result<Value> {
    admission(config, now, false, None, 5432)
}
pub fn live_check(config: &Value, socket: &Path, port: u16, now: Option<i64>) -> Result<Value> {
    let settings = policy(config)?;
    let accepted = admission_lease(config, now, true, Some(socket), port)?;
    let source = durable::read_json(&absolute(&source_snapshot_path(config, settings)?)?)?;
    if super::recovery_repository::source_local(settings)? {
        validate_capture_source(config, settings, &source, clock(now)?)?;
    }
    if records(config, settings, socket, port, &accepted.fds())? != source["records"] {
        return Err(invalid(
            "live primary records differ from the accepted recovery snapshot",
        ));
    }
    let mut result = accepted.result.clone();
    result["snapshot_sha256"] = json!(digest(&source_snapshot_path(config, settings)?)?);
    Ok(result)
}
pub fn import_off_host(config: &Value, incoming: &Path, now: Option<i64>) -> Result<Value> {
    import_off_host_leased(config, incoming, now, &[])
}
fn import_off_host_leased(
    config: &Value,
    incoming: &Path,
    now: Option<i64>,
    outer: &[RawFd],
) -> Result<Value> {
    let settings = policy(config)?;
    let destination = path(settings, "off_host_receipt_file")?;
    let now = clock(now)?;
    let fence = writer_exclusion_leased(config, None, 5432, outer)?;
    let (backup_lease, evidence_lease) = backup_evidence_leases(settings, false)?;
    let mut leases = fds(&fence, &backup_lease, &evidence_lease);
    leases.extend_from_slice(outer);
    let (directory, binding) = backup(config, settings, now, &leases)?;
    let mut local = settings.clone();
    local["off_host_receipt_file"] = Value::Null;
    check_evidence(
        config,
        &local,
        (&directory, &binding),
        now,
        true,
        fence.binding.as_ref(),
        &leases,
    )?;
    let source = snapshot_evidence(config, settings, &binding, now)?;
    let receipt = evidence(
        incoming,
        "off-host restore acceptance",
        &binding,
        settings,
        now,
    )?;
    validate_receipt(
        config,
        settings,
        &source,
        &binding,
        &receipt,
        "off-host restore acceptance",
    )?;
    durable::write_json(&absolute(&destination)?, &receipt)?;
    Ok(receipt)
}
fn preparation_command(preparation: &Value, key: &str, leases: &[RawFd]) -> Result<()> {
    let argv = preparation[key]
        .as_array()
        .ok_or_else(|| invalid(format!("{key} must be explicit absolute executable argv")))?
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| invalid("invalid preparation argv"))
        })
        .collect::<Result<Vec<_>>>()?;
    if !argv.first().is_some_and(|s| Path::new(s).is_absolute()) {
        return Err(invalid(format!(
            "{key} must be explicit absolute executable argv"
        )));
    }
    let mut spec = process::CommandSpec::new(argv);
    spec.leases = leases.to_vec();
    // Managed preparation units deliberately use TimeoutStartSec=infinity.
    // Backup, restore and export are not short probes; their owner controls
    // cancellation while these workers retain the preparation leases.
    spec.timeout = Duration::MAX;
    process::execute(&spec)?;
    Ok(())
}
pub fn prepare(config: &Value, preparation: &Value, socket: &Path, port: u16) -> Result<Value> {
    prepare_at(config, preparation, socket, port, None)
}
/// Explicit clock injection for deterministic bootstrap retry validation.
pub fn prepare_at(
    config: &Value,
    preparation: &Value,
    socket: &Path,
    port: u16,
    now: Option<i64>,
) -> Result<Value> {
    let settings = policy(config)?;
    // Validate all commands before executing any side effect.
    for key in [
        "readiness_command",
        "backup_command",
        "restore_command",
        "export_command",
    ] {
        if key == "export_command"
            && preparation
                .get(key)
                .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty))
        {
            continue;
        }
        let argv = preparation[key]
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid(format!("{key} must be explicit absolute executable argv")))?;
        if !argv[0].as_str().is_some_and(|s| Path::new(s).is_absolute())
            || argv.iter().any(|v| {
                !v.as_str()
                    .is_some_and(|s| !s.is_empty() && !s.contains('\0'))
            })
        {
            return Err(invalid(format!(
                "{key} must be explicit absolute executable argv"
            )));
        }
    }
    let anchor = absolute(&path(settings, "snapshot_file")?)?
        .parent()
        .unwrap()
        .join("preparation.lock");
    let fence = writer_exclusion(config, Some(socket), port)?;
    if super::recovery_repository::source_local(settings)? {
        let backup_lease = backup_lease(settings)?;
        let now = clock(now)?;
        if !exists(&absolute(&path(settings, "backup_root")?)?.join("recovery/SELECTED")) {
            return Err(invalid(
                "source-local preparation requires an explicitly published producer capture",
            ));
        }
        let mut held = vec![backup_lease.fd()];
        if let Some(lease) = &fence.lease {
            held.push(lease.fd());
        }
        let evidence_lease = evidence_lease(settings, true)?;
        held.push(evidence_lease.fd());
        let (_, binding) = backup(config, settings, now, &held)?;
        snapshot_evidence(config, settings, &binding, now)?;
    }
    let preparation_lease = durable::lock(&anchor, false, true)?;
    let mut leases = vec![preparation_lease.fd()];
    if let Some(lease) = &fence.lease {
        leases.push(lease.fd());
    }
    pg_core::inspect_live_leased(
        config,
        string(settings, "system_identifier")?,
        socket,
        port,
        &leases,
    )?;
    preparation_command(preparation, "readiness_command", &leases)?;
    let source_snapshot = if super::recovery_repository::source_local(settings)? {
        let _backup_lease = backup_lease(settings)?;
        source_snapshot_path(config, settings)?
    } else {
        path(settings, "snapshot_file")?
    };
    if super::recovery_repository::source_local(settings)? && !exists(&source_snapshot) {
        return Err(invalid(
            "source-local preparation requires an explicitly published producer snapshot",
        ));
    }
    if !exists(&source_snapshot) {
        let journal = anchor.with_file_name("preparation.json");
        if !exists(&journal) {
            for key in ["receipt_file", "off_host_receipt_file"] {
                if let Some(receipt) = settings[key].as_str()
                    && exists(Path::new(receipt))
                {
                    return Err(invalid(
                        "recovery receipts exist without their bound source snapshot",
                    ));
                }
            }
            preparation_command(preparation, "backup_command", &leases)?;
        }
        let backup_lease = backup_lease(settings)?;
        let mut held = leases.clone();
        held.push(backup_lease.fd());
        let (_, binding) = backup(config, settings, clock(now)?, &held)?;
        if exists(&journal) {
            if durable::read_json(&absolute(&journal)?)? != binding {
                return Err(invalid(
                    "managed preparation backup changed before snapshot; refusing replacement",
                ));
            }
        } else {
            durable::write_json(&journal, &binding)?;
        }
        snapshot_leased(config, socket, port, now, &held)?;
    } else {
        let (backup_lease, evidence_lease) = backup_evidence_leases(settings, true)?;
        let now = clock(now)?;
        let mut held = fds(&fence, &backup_lease, &evidence_lease);
        held.extend_from_slice(&leases);
        let (directory, binding) = backup(config, settings, now, &held)?;
        let source = snapshot_evidence(config, settings, &binding, now)?;
        if settings["require_writer_fence"] == true
            && source_fence(settings, &source)?
                != fence.binding.as_ref().and_then(|v| v["token"].as_str())
        {
            return Err(invalid(
                "record snapshot writer fence differs from the retained window",
            ));
        }
        verify_backup(config, &directory, &held)?;
    }
    if !exists(&path(settings, "receipt_file")?) {
        preparation_command(preparation, "restore_command", &leases)?;
    }
    {
        let (backup_lease, evidence_lease) = backup_evidence_leases(settings, true)?;
        let now = clock(now)?;
        let mut held = fds(&fence, &backup_lease, &evidence_lease);
        held.extend_from_slice(&leases);
        let (directory, binding) = backup(config, settings, now, &held)?;
        let mut local = settings.clone();
        local["off_host_receipt_file"] = Value::Null;
        check_evidence(
            config,
            &local,
            (&directory, &binding),
            now,
            true,
            fence.binding.as_ref(),
            &held,
        )?;
    }
    if preparation["export_command"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        preparation_command(preparation, "export_command", &leases)?;
    }
    if let Some(credentials) = std::env::var_os("CREDENTIALS_DIRECTORY") {
        let incoming = PathBuf::from(credentials).join("recovery-off-host");
        if exists(&incoming) {
            import_off_host_leased(config, &incoming, now, &leases)?;
        }
    }
    Ok(admission_with_outer(config, now, true, None, 5432, &leases)?.result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_backup_outlives_the_short_probe_deadline() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("completed");
        let anchor = root.path().join("preparation.lock");
        let lease = durable::lock(&anchor, false, true).unwrap();
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("sh"))
            .find(|path| path.is_file())
            .unwrap();
        let preparation = json!({"backup_command":[shell,"-c","sleep 61; printf 'completed' > \"$1\"","backup",ready]});
        preparation_command(&preparation, "backup_command", &[lease.fd()]).unwrap();
        assert_eq!(fs::read(&ready).unwrap(), b"completed");
        assert!(durable::lock(&anchor, false, false).is_err());
    }
}
