//! Resumable backend publication. Writer release is a separate durable operation.
use super::{
    Result, application_backup, codec,
    custody::{self, array, paths},
    durable::{self, Lease},
    invalid, resource, string, transition_manifest as manifest,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::{fd::RawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

pub fn journal_path(config: &Value) -> Result<PathBuf> {
    let (source, _) = manifest::validate(config)?;
    Ok(Path::new(string(&source, "state_dir")?).join("transition.json"))
}
pub fn save(config: &Value, record: &mut Value, phase: Option<&str>) -> Result<()> {
    if let Some(phase) = phase {
        record["phase"] = json!(phase);
    }
    manifest::write_owned(&journal_path(config)?, record)
}
pub fn status(config: &Value) -> Result<Value> {
    let record = durable::read_json(&journal_path(config)?)?;
    if record["version"] != 1 || record["intent"] != manifest::intent(config)? {
        return Err(invalid(
            "transition intent or executable identity changed; use the original manifest",
        ));
    }
    Ok(record)
}
pub struct Transaction {
    pub record: Value,
    pub source: Value,
    pub target: Value,
    pub leases: Vec<Lease>,
    pub fds: Vec<RawFd>,
}
fn release_pending(config: &Value) -> Result<bool> {
    match fs::symlink_metadata(Path::new(string(config, "barrier_dir")?).join("inhibited.json")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn terminal(config: &Value, record: &Value) -> Result<bool> {
    Ok(match record["phase"].as_str() {
        Some("write-enabled") => !release_pending(config)?,
        Some("complete" | "aborted") => true,
        _ => false,
    })
}
pub fn pin_source(config: &Value, record: &Value) -> Result<Option<Lease>> {
    if record.get("backup").is_none() || terminal(config, record)? {
        return Ok(None);
    }
    let backup = durable::read_config_json(Path::new(string(config, "backup_manifest")?))?;
    Ok(Some(durable::lock(
        &Path::new(string(&backup, "root")?).join("lock"),
        true,
        false,
    )?))
}
pub fn transaction(config: &Value, held: Option<RawFd>) -> Result<Transaction> {
    manifest::require_root()?;
    let mut leases = vec![];
    let mut fds = vec![];
    if let Some(fd) = held {
        fds.push(fd);
    } else {
        let lease = durable::lock(
            &Path::new(string(config, "barrier_dir")?).join("lock"),
            false,
            false,
        )?;
        fds.push(lease.fd());
        leases.push(lease);
    }
    let record = status(config)?;
    let (source, target) = manifest::validate(config)?;
    let lease = durable::lock(
        &Path::new(string(&source, "state_dir")?).join("lock"),
        terminal(config, &record)?,
        false,
    )?;
    fds.push(lease.fd());
    leases.push(lease);
    if let Some(lease) = pin_source(config, &record)? {
        fds.push(lease.fd());
        leases.push(lease);
    }
    Ok(Transaction {
        record,
        source,
        target,
        leases,
        fds,
    })
}
pub fn plan(config: &Value, candidate: &str, writer_fence_token: Option<&str>) -> Result<Value> {
    manifest::require_root()?;
    let kind = manifest::candidate_path(candidate, config)?;
    let (source, _) = manifest::validate(config)?;
    if !config["postgres_manifest"].is_null()
        && !writer_fence_token.is_some_and(|s| custody::hex(s, 32))
    {
        return Err(invalid(
            "PostgreSQL transition requires the borrowed capability-3 fence token",
        ));
    }
    if config["postgres_manifest"].is_null() && writer_fence_token.is_some() {
        return Err(invalid(
            "a non-PostgreSQL transition cannot borrow an undeclared fence",
        ));
    }
    let path = journal_path(config)?;
    let _lease = durable::lock(
        &Path::new(string(config, "barrier_dir")?).join("lock"),
        false,
        !path.exists(),
    )?;
    if path.exists() {
        let record = status(config)?;
        if record["candidate"] != candidate && record["preparation_contract"] != candidate
            || record["writer_fence_token"] != json!(writer_fence_token)
        {
            return Err(invalid(
                "transition resume candidate or fence identity differs",
            ));
        }
        return Ok(record);
    }
    let (_lease, authority) = resource::inspection(&source)?;
    let mut record = json!({"version":1,"phase":"planned","intent":manifest::intent(config)?,"candidate":if kind=="generation"{Some(candidate)}else{None},"preparation_contract":if kind=="contract"{Some(candidate)}else{None},"barrier_candidate":if kind=="generation"{Some(candidate)}else{None},"writer_fence_token":writer_fence_token,"source_authority":authority,"source_generation":manifest::generation()?,"created_at":custody::now(),"fence":null});
    if !config["custody_manifest"].is_null() {
        let entry = durable::read_config_json(Path::new(string(config, "custody_manifest")?))?;
        let custody = Path::new(string(&entry, "custody_file")?);
        if custody.parent() != Some(Path::new(string(&source, "state_dir")?)) {
            return Err(invalid(
                "target custody publication must stay in the established authority directory",
            ));
        }
        record["source_custody"] = if custody.exists() {
            durable::read_json(custody)?
        } else {
            Value::Null
        };
    }
    save(config, &mut record, None)?;
    Ok(record)
}
fn owner(path: &Path) -> Result<String> {
    super::accounts::name(
        fs::metadata(path)?.uid(),
        16384,
        "authority owner account is absent",
        "invalid account name",
    )
}
fn backup_worker(config: &Value, backup: &Path, _leases: &[RawFd]) -> Result<Value> {
    application_backup::inspect(
        &durable::read_config_json(Path::new(string(config, "backup_manifest")?))?,
        backup,
        None,
    )
}
pub fn capture_source(
    config: &Value,
    record: &Value,
    leases: &[RawFd],
) -> Result<(PathBuf, Value)> {
    let backup_config = durable::read_config_json(Path::new(string(config, "backup_manifest")?))?;
    let (source, _) = manifest::validate(config)?;
    let hash = codec::digest(&codec::encode(&record["intent"], true)?);
    let attempt = format!("transition-{}", &hash[..32]);
    let backup = Path::new(string(&backup_config, "root")?).join(&attempt);
    if !backup.exists() {
        manifest::worker(
            config,
            &json!({"user":owner(Path::new(string(&source,"state_dir")?))?,"argv":[Path::new(string(config,"storage_package")?).join("harbor-db-application-backup"),"--config",config["backup_manifest"],"capture","--attempt",attempt,"--retry-incomplete"]}),
            &BTreeMap::new(),
            leases,
        )?;
    }
    let result = backup_worker(config, &backup, leases)?;
    if result["consistency"] != "quiesced" {
        return Err(invalid(
            "cutover capture requires quiesced sequence and filesystem state",
        ));
    }
    Ok((backup, result))
}
pub fn source_bytes(config: &Value, record: &Value, leases: &[RawFd]) -> Result<Value> {
    let backup = Path::new(string(record, "backup")?);
    let result = backup_worker(config, backup, leases)?;
    if record["source_acceptance_sha256"] != codec::file_digest(&backup.join("acceptance.json"))? {
        return Err(invalid("source backup evidence changed"));
    }
    Ok(result)
}
pub fn run_action(config: &Value, stage: &str, record: &Value, leases: &[RawFd]) -> Result<Value> {
    if record["candidate"].is_null()
        && array(&config["commands"][stage], "argv")?.contains(&json!("{candidate}"))
    {
        return Err(invalid(
            "preparation workers must use the immutable target contract before a generation is bound",
        ));
    }
    let mut substitutions = BTreeMap::new();
    for (key, object, field) in [
        ("{backup}", record, "backup"),
        ("{candidate}", record, "candidate"),
        ("{source}", config, "source_manifest"),
        ("{target}", config, "target_manifest"),
    ] {
        if let Some(v) = object[field].as_str() {
            substitutions.insert(key.into(), v.into());
        }
    }
    manifest::worker(config, &config["commands"][stage], &substitutions, leases)
}
pub fn semantic(receipt: &Value, expected: &str) -> Result<()> {
    if !receipt.as_object().is_some_and(|o| {
        o.len() == 3
            && ["version", "status", "semantic_sha256"]
                .iter()
                .all(|k| o.contains_key(*k))
    }) || receipt["version"] != 1
        || receipt["status"] != "verified"
        || receipt["semantic_sha256"] != expected
    {
        return Err(invalid(
            "complete application semantic evidence differs from the captured source",
        ));
    }
    Ok(())
}
pub fn evidence(config: &Value, record: &Value, leases: &[RawFd]) -> Result<(Value, String)> {
    let source = source_bytes(config, record, leases)?;
    let receipt = durable::read_json(Path::new(string(config, "independent_receipt")?))?;
    if receipt["version"] != 1
        || receipt["status"] != "verified"
        || receipt["resource"] != config["resource"]
        || receipt["source_acceptance_sha256"] != record["source_acceptance_sha256"]
        || ["semantic_sha256", "manifest_sha256", "executables"]
            .iter()
            .any(|k| receipt[*k] != source[*k])
        || receipt["executor_machine_sha256"] == source["executor_machine_sha256"]
        || receipt["executor"] == source["executor"]
    {
        return Err(invalid(
            "independent application restore evidence differs or is not independent",
        ));
    }
    let age = custody::now()
        .checked_sub(
            receipt["certified_at"]
                .as_i64()
                .ok_or_else(|| invalid("invalid independent restoration time"))?,
        )
        .ok_or_else(|| invalid("invalid independent restoration time"))?;
    let backup = durable::read_config_json(Path::new(string(config, "backup_manifest")?))?;
    if age < 0
        || age
            > backup["maximum_age_seconds"]
                .as_i64()
                .ok_or_else(|| invalid("invalid backup maximum age"))?
    {
        return Err(invalid(
            "independent restoration is stale or from the future",
        ));
    }
    let hash = codec::file_digest(Path::new(string(config, "independent_receipt")?))?;
    if !record["independent_sha256"].is_null() && record["independent_sha256"] != hash {
        return Err(invalid(
            "independent restore evidence changed during resume",
        ));
    }
    Ok((source, hash))
}
pub fn primary_evidence(config: &Value, record: &Value, leases: &[RawFd]) -> Result<Value> {
    if config["postgres_manifest"].is_null() {
        return Ok(Value::Null);
    }
    let result = manifest::worker(
        config,
        &json!({"user":"postgres","argv":[Path::new(string(config,"storage_package")?).join("harbor-db-postgres"),"--config",config["postgres_manifest"],"inspect-recovery","--socket-dir",config["postgres_socket"],"--port",config["postgres_port"].to_string()]}),
        &BTreeMap::new(),
        leases,
    )?;
    if result["status"] != "ready" {
        return Err(invalid(
            "the whole primary recovery boundary is not accepted",
        ));
    }
    let database = durable::read_config_json(Path::new(string(config, "postgres_manifest")?))?;
    let path = super::recovery::source_snapshot_path(&database, &database["recovery"])?;
    let snapshot = durable::read_json(&path)?;
    if snapshot["writer_fence_token"] != record["writer_fence_token"]
        || result["snapshot_sha256"] != codec::file_digest(&path)?
    {
        return Err(invalid(
            "primary snapshot does not bind the same held writer fence",
        ));
    }
    if !record["primary_snapshot_sha256"].is_null()
        && record["primary_snapshot_sha256"] != result["snapshot_sha256"]
    {
        return Err(invalid(
            "primary recovery snapshot changed during transition resume",
        ));
    }
    Ok(result["snapshot_sha256"].clone())
}
pub fn target_custody(
    config: &Value,
    record: &Value,
    target: &Value,
    leases: &[RawFd],
) -> Result<Value> {
    if config["custody_manifest"].is_null() {
        return Ok(Value::Null);
    }
    let entry = durable::read_config_json(Path::new(string(config, "custody_manifest")?))?;
    if entry["kind"] != "filesystem" || entry["authority"] != *target {
        return Err(invalid(
            "target custody manifest does not declare the exact target authority",
        ));
    }
    let mut requirements = vec![];
    let empty = vec![];
    let checks = match entry.get("database_inventory_checks") {
        None => &empty,
        Some(value) => value
            .as_array()
            .ok_or_else(|| invalid("database_inventory_checks must be an array"))?,
    };
    if !checks.is_empty() && config["postgres_manifest"].is_null() {
        return Err(invalid(
            "database-bound custody requires the borrowed PostgreSQL fence",
        ));
    }
    for check in checks {
        let database = durable::read_config_json(Path::new(string(config, "postgres_manifest")?))?;
        let command = json!({"user":"postgres","argv":[Path::new(string(&database,"package")?).join("bin/psql"),"-X","-w","-qAt","-v","ON_ERROR_STOP=1","-h",config["postgres_socket"],"-p",config["postgres_port"].to_string(),"-U","postgres","-d",check["database"],"-c",format!("BEGIN READ ONLY; SET LOCAL statement_timeout = '30s'; {}; COMMIT;",string(check,"sql")?)]});
        let result = manifest::worker(config, &command, &BTreeMap::new(), leases)?;
        for item in result
            .as_array()
            .ok_or_else(|| invalid("database corpus query did not produce a path array"))?
        {
            let mut requirement = json!({"root":check["root"]});
            for (k, v) in item
                .as_object()
                .ok_or_else(|| invalid("invalid corpus path"))?
            {
                requirement[k] = v.clone();
            }
            requirements.push(requirement);
        }
    }
    let contents = custody::inventory(&entry, true)?;
    let requirements = json!(requirements);
    custody::require_database_paths(
        &contents,
        &requirements,
        &paths(target, "directories")?,
        entry["git_executable"].as_str(),
    )?;
    Ok(
        json!({"version":1,"resource":target["resource"],"identity":record["source_authority"]["identity"],"binding":target["binding"],"directories":target["directories"],"root_identities":custody::root_identities(&entry)?,"inventory":contents,"metadata":custody::inventory(&entry,false)?,"completed_at":custody::now(),"database_snapshot_sha256":record["primary_snapshot_sha256"],"database_requirements":requirements,"transition_source_acceptance_sha256":record["source_acceptance_sha256"],"independent_restore_sha256":record["independent_sha256"],"semantic_sha256":record["semantic_sha256"]}),
    )
}
fn custody_equal(observed: &Value, expected: &Value) -> bool {
    expected.as_object().is_some_and(|o| {
        o.iter()
            .all(|(k, v)| k == "completed_at" || observed[k] == *v)
    })
}
pub fn verify_custody(
    config: &Value,
    record: &Value,
    target: &Value,
    leases: &[RawFd],
) -> Result<()> {
    let observed = target_custody(config, record, target, leases)?;
    if !record["custody"].is_null() && !custody_equal(&observed, &record["custody"]) {
        return Err(invalid("prepared target corpus evidence changed"));
    }
    Ok(())
}
pub fn prepare(config: &Value) -> Result<Value> {
    manifest::require_root()?;
    let lease = durable::lock(
        &Path::new(string(config, "barrier_dir")?).join("lock"),
        false,
        false,
    )?;
    let mut record = status(config)?;
    if ![
        "planned",
        "quiescing",
        "quiesced",
        "captured",
        "importing",
        "imported",
        "prepared",
    ]
    .contains(&string(&record, "phase")?)
    {
        return Err(invalid(
            "transition preparation is not permitted at this phase",
        ));
    }
    let phase = if record["phase"] == "planned" {
        Some("quiescing")
    } else {
        None
    };
    save(config, &mut record, phase)?;
    manifest::install_barriers(config, &record)?;
    manifest::stop_units(config)?;
    prepare_locked(config, lease.fd())
}
pub fn prepare_locked(config: &Value, transition_lease: RawFd) -> Result<Value> {
    let mut tx = transaction(config, Some(transition_lease))?;
    let fence = manifest::fence(config, &mut tx.record, &tx.fds)?;
    manifest::inspect_barriers(config)?;
    if resource::verify(&tx.source, &resource::contract(&tx.source)?)?
        != tx.record["source_authority"]
    {
        return Err(invalid("old source authority changed during transition"));
    }
    if matches!(tx.record["phase"].as_str(), Some("quiescing" | "quiesced")) {
        save(config, &mut tx.record, Some("quiesced"))?;
        let (backup, accepted) = capture_source(config, &tx.record, &fence.fds)?;
        tx.record["backup"] = json!(backup);
        tx.record["source_acceptance_sha256"] =
            json!(codec::file_digest(&backup.join("acceptance.json"))?);
        tx.record["semantic_sha256"] = accepted["semantic_sha256"].clone();
        save(config, &mut tx.record, Some("captured"))?;
    }
    let pin = pin_source(config, &tx.record)?;
    let mut fds = fence.fds.clone();
    if let Some(lease) = &pin {
        fds.push(lease.fd());
    }
    prepare_target(config, &mut tx.record, &tx.target, &fds)
}
pub fn prepare_target(
    config: &Value,
    record: &mut Value,
    target: &Value,
    leases: &[RawFd],
) -> Result<Value> {
    if !Path::new(string(config, "independent_receipt")?).exists() {
        let mut pending = record.clone();
        pending["status"] = json!("awaiting-independent-restore");
        return Ok(pending);
    }
    let (_, hash) = evidence(config, record, leases)?;
    record["independent_sha256"] = json!(hash);
    if !matches!(record["phase"].as_str(), Some("imported" | "prepared")) {
        save(config, record, Some("importing"))?;
        run_action(config, "import", record, leases)?;
    }
    semantic(
        &run_action(config, "verify-target", record, leases)?,
        string(record, "semantic_sha256")?,
    )?;
    evidence(config, record, leases)?;
    if record["phase"] != "prepared" {
        save(config, record, Some("imported"))?;
    }
    record["primary_snapshot_sha256"] = primary_evidence(config, record, leases)?;
    let mut prepared = resource::contract(target)?;
    prepared["identity"] = record["source_authority"]["identity"].clone();
    if !record["target_authority"].is_null() && record["target_authority"] != prepared {
        return Err(invalid("prepared target authority evidence changed"));
    }
    record["target_authority"] = prepared;
    let custody = target_custody(config, record, target, leases)?;
    if !record["custody"].is_null() && !custody_equal(&custody, &record["custody"]) {
        return Err(invalid("prepared target corpus evidence changed"));
    }
    if record.get("custody").is_none() {
        record["custody"] = custody;
    }
    save(config, record, Some("prepared"))?;
    Ok(record.clone())
}
pub fn publish(_config: &Value, target: &Value, expected: &Value, old: &Value) -> Result<()> {
    let state = resource::state_directory(target)?;
    let current = durable::read_json(&state.join("identity.json"))?;
    if current != *old && current != *expected {
        return Err(invalid(
            "authority compare-and-swap rejected a different live identity",
        ));
    }
    let marker = json!({"resource":target["resource"],"identity":expected["identity"]});
    for directory in paths(expected, "directories")? {
        let path = resource::anchor(target, &directory)?;
        if path.exists() && durable::read_json(&path)? != marker {
            return Err(invalid("target storage has a conflicting identity"));
        }
        manifest::write_owned(&path, &marker)?;
    }
    manifest::write_owned(&state.join("identity.json"), expected)
}
fn verify_prepared(config: &Value, tx: &Transaction, leases: &[RawFd]) -> Result<()> {
    manifest::inspect_barriers(config)?;
    evidence(config, &tx.record, leases)?;
    primary_evidence(config, &tx.record, leases)?;
    semantic(
        &run_action(config, "verify-target", &tx.record, leases)?,
        string(&tx.record, "semantic_sha256")?,
    )?;
    verify_custody(config, &tx.record, &tx.target, leases)
}
pub fn commit(config: &Value) -> Result<Value> {
    let mut tx = transaction(config, None)?;
    let fence = manifest::fence(config, &mut tx.record, &tx.fds)?;
    if !matches!(
        tx.record["phase"].as_str(),
        Some("prepared" | "committing" | "committed")
    ) {
        return Err(invalid("authority commit requires a prepared transition"));
    }
    manifest::inspect_barriers(config)?;
    if tx.record["candidate"] != manifest::generation()? {
        return Err(invalid("the exact accepted generation is not selected"));
    }
    verify_prepared(config, &tx, &fence.fds)?;
    if tx.record["phase"] != "committed" {
        save(config, &mut tx.record, Some("committing"))?;
        publish(
            config,
            &tx.target,
            &tx.record["target_authority"],
            &tx.record["source_authority"],
        )?;
        if !tx.record["custody"].is_null() {
            let entry = durable::read_config_json(Path::new(string(config, "custody_manifest")?))?;
            manifest::write_owned(
                Path::new(string(&entry, "custody_file")?),
                &tx.record["custody"],
            )?;
        }
        save(config, &mut tx.record, Some("committed"))?;
    }
    resource::verify(&tx.target, &resource::contract(&tx.target)?)?;
    Ok(tx.record)
}
pub fn bind_candidate(config: &Value, candidate: &str) -> Result<Value> {
    let mut tx = transaction(config, None)?;
    if tx.record["phase"] != "prepared"
        || manifest::candidate_path(candidate, config)? != "generation"
    {
        return Err(invalid(
            "bind the realized generation only after transition preparation",
        ));
    }
    if !tx.record["candidate"].is_null() && tx.record["candidate"] != candidate {
        return Err(invalid("a different generation is already bound"));
    }
    manifest::inspect_barriers(config)?;
    evidence(config, &tx.record, &tx.fds)?;
    tx.record["candidate"] = json!(candidate);
    save(config, &mut tx.record, None)?;
    Ok(tx.record)
}
pub fn admission(
    config: &Value,
    phase: &str,
    expected: &Value,
    candidate: Option<&str>,
) -> Result<Value> {
    let record = status(config)?;
    if !matches!(
        record["phase"].as_str(),
        Some("prepared" | "committing" | "committed")
    ) || !matches!(phase, "preflight" | "activate")
    {
        return Err(invalid(
            "prepared transition cannot authorize ordinary startup or incomplete preparation",
        ));
    }
    let (_, target) = manifest::validate(config)?;
    if target != *expected {
        return Err(invalid(
            "candidate transition authority differs from the prepared target contract",
        ));
    }
    if phase == "activate" && record["candidate"].is_null() {
        return Err(invalid(
            "activation requires an explicitly bound realized generation",
        ));
    }
    if phase == "activate" && record["candidate"] != json!(candidate) {
        return Err(invalid(
            "activation generation differs from the bound candidate",
        ));
    }
    let mut tx = transaction(config, None)?;
    let fence = manifest::fence(config, &mut tx.record, &tx.fds)?;
    verify_prepared(config, &tx, &fence.fds)?;
    if matches!(tx.record["phase"].as_str(), Some("prepared" | "committing")) {
        let current =
            durable::read_json(&Path::new(string(&tx.source, "state_dir")?).join("identity.json"))?;
        if current != tx.record["source_authority"] && current != tx.record["target_authority"] {
            return Err(invalid("prepared transition authority changed"));
        }
    } else {
        resource::verify(&tx.target, &resource::contract(&tx.target)?)?;
    }
    Ok(
        json!({"status":"prepared-with-writers-inhibited","resource":config["resource"],"candidate":tx.record["candidate"],"semantic_sha256":tx.record["semantic_sha256"],"database_snapshot_sha256":tx.record["primary_snapshot_sha256"],"database_requirements":if tx.record["custody"].is_null(){json!([])}else{tx.record["custody"]["database_requirements"].clone()}}),
    )
}
pub fn enable_writes(config: &Value) -> Result<Value> {
    let mut tx = transaction(config, None)?;
    if !matches!(
        tx.record["phase"].as_str(),
        Some("committed" | "write-enabled")
    ) {
        return Err(invalid("writer release requires committed authority"));
    }
    if tx.record["candidate"] != manifest::generation()? {
        return Err(invalid("writer release generation differs"));
    }
    if tx.record["phase"] != "write-enabled" || release_pending(config)? {
        let fence = manifest::fence(config, &mut tx.record, &tx.fds)?;
        verify_prepared(config, &tx, &fence.fds)?;
        resource::verify(&tx.target, &resource::contract(&tx.target)?)?;
        if tx.record["phase"] != "write-enabled" {
            save(config, &mut tx.record, Some("write-enabled"))?;
        }
        manifest::release_barriers(config, &tx.record, false)?;
    } else {
        manifest::release_barriers(config, &tx.record, false)?;
    }
    tx.record["status"] = json!("write-enabled");
    tx.record["borrowed_fence_release_required"] = json!(!config["postgres_manifest"].is_null());
    Ok(tx.record)
}
pub fn complete(config: &Value) -> Result<Value> {
    manifest::require_root()?;
    let lease = durable::lock(
        &Path::new(string(config, "barrier_dir")?).join("lock"),
        false,
        false,
    )?;
    let mut record = status(config)?;
    if !matches!(record["phase"].as_str(), Some("write-enabled" | "complete"))
        || record["candidate"] != manifest::generation()?
    {
        return Err(invalid(
            "completion requires the selected write-enabled generation",
        ));
    }
    if release_pending(config)? {
        return Err(invalid("completion requires released writer barriers"));
    }
    let (_, target) = manifest::validate(config)?;
    let _inspection = resource::inspection(&target)?;
    if run_action(config, "health", &record, &[lease.fd()])?
        != json!({"version":1,"status":"healthy"})
    {
        return Err(invalid("target application health acceptance failed"));
    }
    save(config, &mut record, Some("complete"))?;
    Ok(record)
}
pub fn abort(config: &Value) -> Result<Value> {
    let mut tx = transaction(config, None)?;
    if tx.record["phase"] == "aborted" {
        manifest::release_barriers(config, &tx.record, true)?;
        return Ok(tx.record);
    }
    if matches!(
        tx.record["phase"].as_str(),
        Some("write-enabled" | "complete")
    ) {
        return Err(invalid(
            "writers may have acknowledged changes; a fresh reverse transition is required",
        ));
    }
    if tx.record["source_generation"] != manifest::generation()? {
        return Err(invalid(
            "abort requires the exact retained source generation selected with writers inhibited",
        ));
    }
    let fence = manifest::fence(config, &mut tx.record, &tx.fds)?;
    if tx.record.get("backup").is_some() {
        source_bytes(config, &tx.record, &fence.fds)?;
        semantic(
            &run_action(config, "verify-source", &tx.record, &fence.fds)?,
            string(&tx.record, "semantic_sha256")?,
        )?;
    }
    if matches!(
        tx.record["phase"].as_str(),
        Some("committing" | "committed" | "aborting")
    ) {
        save(config, &mut tx.record, Some("aborting"))?;
        publish(
            config,
            &tx.source,
            &tx.record["source_authority"],
            &tx.record["target_authority"],
        )?;
        if !tx.record["custody"].is_null() {
            let entry = durable::read_config_json(Path::new(string(config, "custody_manifest")?))?;
            let path = Path::new(string(&entry, "custody_file")?);
            let current = if path.exists() {
                durable::read_json(path)?
            } else {
                Value::Null
            };
            if current != tx.record["custody"] && current != tx.record["source_custody"] {
                return Err(invalid(
                    "custody compare-and-swap rejected changed evidence",
                ));
            }
            if !tx.record["source_custody"].is_null() {
                manifest::write_owned(path, &tx.record["source_custody"])?;
            } else if path.exists() {
                fs::remove_file(path)?;
                durable::sync_directory(
                    path.parent()
                        .ok_or_else(|| invalid("missing custody parent"))?,
                )?;
            }
        }
    }
    resource::verify(&tx.source, &resource::contract(&tx.source)?)?;
    save(config, &mut tx.record, Some("aborted"))?;
    manifest::release_barriers(config, &tx.record, true)?;
    Ok(tx.record)
}
pub fn retire(config: &Value) -> Result<Value> {
    let tx = transaction(config, None)?;
    if !matches!(tx.record["phase"].as_str(), Some("complete" | "aborted")) {
        return Err(invalid("unfinished transitions cannot be retired"));
    }
    if release_pending(config)? {
        return Err(invalid(
            "unfinished writer barrier release cannot be retired",
        ));
    }
    let active = if tx.record["phase"] == "complete" {
        &tx.target
    } else {
        &tx.source
    };
    resource::verify(active, &resource::contract(active)?)?;
    let hash = codec::digest(&codec::encode(&tx.record["intent"], true)?);
    let archive = Path::new(string(config, "barrier_dir")?).join(format!("{hash}.history.json"));
    if archive.exists() && durable::read_json(&archive)? != tx.record {
        return Err(invalid("transition history conflicts"));
    }
    durable::write_json(&archive, &tx.record)?;
    let journal = journal_path(config)?;
    fs::remove_file(&journal)?;
    durable::sync_directory(
        journal
            .parent()
            .ok_or_else(|| invalid("missing journal parent"))?,
    )?;
    Ok(json!({"status":"retired","history":archive}))
}
