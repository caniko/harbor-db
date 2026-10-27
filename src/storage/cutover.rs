//! Mandatory read-only deployment admission and explicit independent custody certification.
pub use super::custody::{
    CertificationContext, certify_filesystem, certify_filesystem_with_context,
    certify_filesystem_with_contract, inventory, require_database_paths, root_identities,
    validate_git_repository, validate_path, writer_active,
};
use super::{
    Result, application_transition, codec, custody, durable, invalid, pg_core, postgres, process,
    recovery, resource, string,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn digest(path: &Path) -> Result<String> {
    codec::file_digest(path)
}
pub fn validate_manifest(value: &Value, host: &str) -> Result<Value> {
    let resources = value
        .get("resources")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("unsupported cutover contract or target host mismatch"))?;
    if value["version"] != 1
        || value["enforced"] != true
        || value["host"] != host
        || !custody::name(host)
    {
        return Err(invalid(
            "unsupported cutover contract or target host mismatch",
        ));
    }
    for (key, default, maximum) in [
        ("timeout_seconds", 30, 300),
        ("activation_timeout_seconds", 900, 3600),
    ] {
        if !value
            .get(key)
            .unwrap_or(&json!(default))
            .as_u64()
            .is_some_and(|n| n > 0 && n <= maximum)
        {
            return Err(invalid(format!(
                "invalid bounded cutover inspection budget: {key}"
            )));
        }
    }
    for (name, entry) in resources {
        if !custody::name(name)
            || !entry.is_object()
            || !matches!(entry["kind"].as_str(), Some("postgres" | "filesystem"))
            || !entry["user"].as_str().is_some_and(custody::name)
        {
            return Err(invalid(format!("invalid cutover resource: {name}")));
        }
        if entry["kind"] == "postgres" {
            validate_path(string(entry, "config")?)?;
            if entry["user"] != "postgres" {
                return Err(invalid(
                    "PostgreSQL admission requires the postgres service identity",
                ));
            }
            if !entry["switch_adoption_identifier"].is_null()
                && !entry["switch_adoption_identifier"]
                    .as_str()
                    .is_some_and(|id| {
                        !id.starts_with('0')
                            && !id.is_empty()
                            && id.bytes().all(|b| b.is_ascii_digit())
                    })
            {
                return Err(invalid("invalid PostgreSQL switch adoption identifier"));
            }
        } else {
            if entry.get("git_executable").is_some() {
                custody::git_executable(string(entry, "git_executable")?)?;
            }
            if !entry["transition_manifest"].is_null() {
                validate_path(string(entry, "transition_manifest")?)?;
            }
            let authority = &entry["authority"];
            let directories = custody::paths(authority, "directories")?;
            let unique: BTreeSet<_> = directories.iter().collect();
            if directories.is_empty()
                || unique.len() != directories.len()
                || authority["resource"] != *name
                || !authority["binding"].is_object()
                || entry["max_age_seconds"].as_u64().is_none_or(|n| n == 0)
            {
                return Err(invalid(format!(
                    "invalid existing filesystem authority: {name}"
                )));
            }
            custody::receipt_path(entry)?;
            for directory in directories {
                validate_path(
                    directory
                        .to_str()
                        .ok_or_else(|| invalid("invalid corpus directory"))?,
                )?;
            }
            if !entry["login_shell"].is_null() {
                validate_path(string(entry, "login_shell")?)?;
            }
            if custody::array(entry, "runtime_units")?
                .iter()
                .any(|u| !u.as_str().is_some_and(custody::unit))
            {
                return Err(invalid(
                    "filesystem custody requires explicit writer service units",
                ));
            }
            if !entry["database_resource"].is_null()
                && resources
                    .get(string(entry, "database_resource")?)
                    .is_none_or(|v| v["kind"] != "postgres")
            {
                return Err(invalid(
                    "filesystem custody requires an enrolled PostgreSQL recovery dependency",
                ));
            }
        }
    }
    let namespaces = namespace_roots(value, None)?;
    for (index, (root, _)) in namespaces.iter().enumerate() {
        if namespaces[index + 1..]
            .iter()
            .any(|(other, _)| root.starts_with(other) || other.starts_with(root))
        {
            return Err(invalid(
                "filesystem custody namespaces must be globally disjoint",
            ));
        }
    }
    Ok(value.clone())
}

pub fn namespace_contracts_until(manifest: &Value, deadline: Instant) -> Result<Value> {
    let mut contracts = serde_json::Map::new();
    let mut encoded_bytes = 2; // Opening and closing object delimiters.
    for (name, entry) in manifest["resources"]
        .as_object()
        .ok_or_else(|| invalid("invalid cutover resources"))?
    {
        if entry["kind"] == "filesystem" {
            let contract = resource::contract_until(&entry["authority"], Some(deadline))?;
            encoded_bytes += serde_json::to_vec(name)?.len()
                + serde_json::to_vec(&contract)?.len()
                + 1 // Key/value separator.
                + usize::from(!contracts.is_empty());
            if encoded_bytes > MAXIMUM_SNAPSHOT_BYTES {
                return Err(invalid(
                    "namespace consumer snapshot exceeds aggregate size limit",
                ));
            }
            contracts.insert(name.clone(), contract);
        }
    }
    let contracts = Value::Object(contracts);
    validate_namespace_contracts(manifest, &contracts)?;
    Ok(contracts)
}

fn validate_namespace_contracts(
    manifest: &Value,
    contracts: &Value,
) -> Result<BTreeMap<String, BTreeSet<PathBuf>>> {
    let mut roots = BTreeMap::new();
    for (name, entry) in manifest["resources"]
        .as_object()
        .ok_or_else(|| invalid("invalid cutover resources"))?
    {
        if entry["kind"] != "filesystem" {
            continue;
        }
        let authority = &entry["authority"];
        let expected = contracts
            .get(name)
            .ok_or_else(|| invalid("namespace inspection is missing an authority contract"))?;
        let directories = roots.entry(name.clone()).or_insert_with(BTreeSet::new);
        directories.extend(custody::paths(expected, "directories")?);
        let state = Path::new(string(authority, "state_dir")?);
        if !state.join("identity.json").exists() {
            continue;
        }
        let _lease = durable::lock(&state.join("lock"), true, false)?;
        if let Some(selected) = prepared_transition(entry, "activate")? {
            let (source, target) = super::transition_manifest::validate(&selected)?;
            if target != *authority {
                return Err(invalid(
                    "namespace transition target differs from authority",
                ));
            }
            let record = application_transition::status(&selected)?;
            let current = durable::read_json(&state.join("identity.json"))?;
            if current == record["target_authority"] {
                resource::verify(authority, expected)?;
            } else if current == record["source_authority"] {
                resource::require_contract(expected, Some(&record["target_authority"]))?;
                resource::verify(&source, &record["source_authority"])?;
            } else {
                return Err(invalid("namespace transition authority changed"));
            }
            // Retain both recorded namespaces under the validation lease. A
            // later mutable identity read must never select the scan's roots.
            directories.extend(custody::paths(&record["source_authority"], "directories")?);
            directories.extend(custody::paths(&record["target_authority"], "directories")?);
        } else {
            resource::require_stable(authority)?;
            resource::verify(authority, expected)?;
        }
    }
    Ok(roots)
}

fn namespace_roots(manifest: &Value, contracts: Option<&Value>) -> Result<Vec<(PathBuf, bool)>> {
    let mut roots = Vec::new();
    let validated = contracts
        .map(|contracts| validate_namespace_contracts(manifest, contracts))
        .transpose()?;
    for (name, entry) in manifest["resources"]
        .as_object()
        .ok_or_else(|| invalid("invalid cutover resources"))?
    {
        if entry["kind"] == "postgres" {
            let database = durable::read_config_json(Path::new(string(entry, "config")?))?;
            for (key, primary) in [("data_dir", true), ("state_dir", false)] {
                let path = string(&database, key)?;
                validate_path(path)?;
                roots.push((PathBuf::from(path), primary));
            }
            if let Some(startup) = database
                .get("startup_inhibition")
                .filter(|value| !value.is_null())
            {
                let path = string(startup, "state_dir")?;
                validate_path(path)?;
                roots.push((PathBuf::from(path), false));
            }
        } else {
            let authority = &entry["authority"];
            roots.push((
                PathBuf::from(string(&entry["authority"], "state_dir")?),
                false,
            ));
            let directories = match &validated {
                Some(validated) => validated[name].clone(),
                None => custody::paths(authority, "directories")?
                    .into_iter()
                    .collect(),
            };
            roots.extend(directories.into_iter().map(|path| (path, false)));
        }
    }
    Ok(roots)
}

/// Global coordinators can inspect all private authorities before dispatching
/// user-scoped workers. Workers/serve retain their own lease and startup guard;
/// they must not require read access to other applications' private corpora.
pub fn validate_storage_namespaces(manifest: &Value) -> Result<()> {
    validate_storage_namespaces_until(
        manifest,
        Instant::now()
            + Duration::from_secs(
                manifest["activation_timeout_seconds"]
                    .as_u64()
                    .unwrap_or(900),
            ),
    )
    .map(|_| ())
}
pub fn validate_storage_namespaces_until(manifest: &Value, deadline: Instant) -> Result<Value> {
    let contracts = namespace_contracts_until(manifest, deadline)?;
    inspect_namespace_roots(namespace_roots(manifest, Some(&contracts))?, deadline)?;
    Ok(contracts)
}

/// Check the complete declared contract even in a user-scoped certifying worker.
pub fn validate_restore_roots(manifest: &Value, restores: &[PathBuf]) -> Result<()> {
    let deadline = Instant::now()
        + Duration::from_secs(
            manifest["activation_timeout_seconds"]
                .as_u64()
                .unwrap_or(900),
        );
    let contracts = namespace_contracts_until(manifest, deadline)?;
    validate_restore_roots_with_contracts(manifest, restores, &contracts)
}

fn validate_restore_roots_with_contracts(
    manifest: &Value,
    restores: &[PathBuf],
    contracts: &Value,
) -> Result<()> {
    let protected = namespace_roots(manifest, Some(contracts))?;
    validate_restore_roots_against(restores, &protected)
}

fn validate_restore_roots_against(
    restores: &[PathBuf],
    protected: &[(PathBuf, bool)],
) -> Result<()> {
    for restore in restores {
        validate_path(&restore.to_string_lossy())?;
        if protected
            .iter()
            .any(|(root, _)| restore.starts_with(root) || root.starts_with(restore))
        {
            return Err(invalid(
                "restore roots must be independent of every enrolled storage namespace",
            ));
        }
    }
    Ok(())
}

/// Both coordinator and certifying child reject complete global inode aliases.
pub fn validate_restore_namespaces(manifest: &Value, restores: &[PathBuf]) -> Result<()> {
    validate_restore_namespaces_with_contracts(
        manifest,
        restores,
        None,
        Instant::now()
            + Duration::from_secs(
                manifest["activation_timeout_seconds"]
                    .as_u64()
                    .unwrap_or(900),
            ),
    )
    .map(|_| ())
}

pub fn validate_restore_namespaces_with_contracts(
    manifest: &Value,
    restores: &[PathBuf],
    contracts: Option<&Value>,
    deadline: Instant,
) -> Result<Value> {
    let contracts = match contracts {
        Some(contracts) if contracts.is_object() => contracts.clone(),
        Some(_) => return Err(invalid("invalid namespace consumer contracts")),
        None => namespace_contracts_until(manifest, deadline)?,
    };
    let mut roots = namespace_roots(manifest, Some(&contracts))?;
    validate_restore_roots_against(restores, &roots)?;
    roots.extend(restores.iter().cloned().map(|root| (root, false)));
    inspect_namespace_roots(roots, deadline)?;
    Ok(contracts)
}

fn inspect_namespace_roots(roots: Vec<(PathBuf, bool)>, deadline: Instant) -> Result<()> {
    let mut protected = BTreeSet::new();
    for (root, primary) in roots {
        if Instant::now() >= deadline {
            return Err(invalid("cutover inspection budget exhausted"));
        }
        match fs::symlink_metadata(&root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        if fs::canonicalize(&root)? != root {
            return Err(invalid("custody namespaces contain redirected storage"));
        }
        let mut identities = BTreeSet::new();
        recovery::storage_identities_until(&root, &mut identities, primary, Some(deadline))?;
        if !protected.is_disjoint(&identities) {
            return Err(invalid(
                "custody namespaces alias shared storage across resource leases",
            ));
        }
        protected.extend(identities);
    }
    Ok(())
}

/// Certifying children inspect private namespaces while privileged, then publish
/// exclusively as the declared service account. Direct user calls do the same
/// global check and fail closed when another enrolled namespace is unreadable.
pub fn enter_certification_user(config: &Value) -> Result<()> {
    let account = process::account(string(config, "user")?)?;
    let uid = unsafe { libc::geteuid() };
    if uid == account.uid {
        return Ok(());
    }
    if uid != 0 {
        return Err(invalid(
            "certification requires the declared resource service user",
        ));
    }
    if unsafe {
        libc::setgroups(account.groups.len(), account.groups.as_ptr()) != 0
            || libc::setgid(account.gid) != 0
            || libc::setuid(account.uid) != 0
    } {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn prepared_transition(config: &Value, phase: &str) -> Result<Option<Value>> {
    let Some(path) = config["transition_manifest"]
        .as_str()
        .filter(|_| matches!(phase, "preflight" | "activate"))
    else {
        return Ok(None);
    };
    let selected = durable::read_config_json(Path::new(path))?;
    let journal = application_transition::journal_path(&selected)?;
    if journal.exists()
        && matches!(
            durable::read_json(&journal)?["phase"].as_str(),
            Some("prepared" | "committing" | "committed")
        )
    {
        Ok(Some(selected))
    } else {
        Ok(None)
    }
}

pub fn check_resource(
    config: &Value,
    phase: &str,
    at: Option<i64>,
    candidate: Option<&str>,
) -> Result<Value> {
    check_resource_with_contract(config, phase, at, candidate, None)
}

pub fn check_resource_with_contract(
    config: &Value,
    phase: &str,
    at: Option<i64>,
    candidate: Option<&str>,
    pinned: Option<&Value>,
) -> Result<Value> {
    check_resource_until(config, phase, at, candidate, pinned, None)
}
pub fn check_resource_until(
    config: &Value,
    phase: &str,
    at: Option<i64>,
    candidate: Option<&str>,
    pinned: Option<&Value>,
    deadline: Option<Instant>,
) -> Result<Value> {
    if !matches!(phase, "preflight" | "activate" | "startup" | "certify") {
        return Err(invalid("unsupported cutover phase"));
    }
    let now = at.unwrap_or_else(custody::now);
    if config["kind"] == "filesystem" {
        if let Some(selected) = prepared_transition(config, phase)? {
            if pinned.is_some() {
                let mut prepared = durable::read_json(&application_transition::journal_path(
                    &selected,
                )?)?["target_authority"]
                    .clone();
                prepared
                    .as_object_mut()
                    .ok_or_else(|| invalid("missing prepared target authority"))?
                    .remove("identity");
                resource::require_contract(&prepared, pinned)?;
            }
            if selected["custody_manifest"].is_null() {
                return Err(invalid(
                    "cutover transition admission requires target corpus custody publication",
                ));
            }
            return application_transition::admission(
                &selected,
                phase,
                &config["authority"],
                candidate,
            );
        }
        let authority = &config["authority"];
        let metadata = if phase != "startup"
            && authority["consumer_command"]
                .as_array()
                .is_none_or(|argv| argv.is_empty())
        {
            Some(inventory(config, false)?)
        } else {
            None
        };
        let (_lease, adopted) = resource::inspection_until(authority, pinned, deadline)?;
        let expanded = custody::expanded_config(config, &adopted);
        let config = &expanded;
        let authority = &config["authority"];
        let metadata = if metadata.is_none() && phase != "startup" {
            Some(inventory(config, false)?)
        } else {
            metadata
        };
        let receipt = durable::read_json(&recovery::absolute(Path::new(string(
            config,
            "custody_file",
        )?))?)?;
        if receipt["version"] != 1
            || receipt["resource"] != authority["resource"]
            || receipt["identity"] != adopted["identity"]
            || receipt["binding"] != authority["binding"]
            || receipt["directories"] != authority["directories"]
            || receipt["root_identities"] != root_identities(config)?
        {
            return Err(invalid(
                "filesystem custody binding differs from the adopted authority",
            ));
        }
        if phase == "startup" {
            return Ok(json!({}));
        }
        recovery::fresh(
            &receipt["completed_at"],
            now,
            config["max_age_seconds"]
                .as_i64()
                .ok_or_else(|| invalid("invalid custody maximum age"))?,
        )?;
        if metadata.as_ref() != receipt.get("metadata") {
            return Err(invalid(
                "source corpus changed; repeat custody certification",
            ));
        }
        let requirements = receipt
            .get("database_requirements")
            .cloned()
            .unwrap_or_else(|| json!([]));
        if phase != "preflight" {
            let source = inventory(config, true)?;
            if source != receipt["inventory"] {
                return Err(invalid(
                    "source corpus contents differ from the certified restore",
                ));
            }
            require_database_paths(
                &source,
                &requirements,
                &custody::paths(authority, "directories")?,
                config["git_executable"].as_str(),
            )?;
        }
        Ok(
            json!({"database_snapshot_sha256":receipt["database_snapshot_sha256"],"database_requirements":requirements}),
        )
    } else {
        let database = durable::read_config_json(Path::new(string(config, "config")?))?;
        // A declared first-rollout switch adoption runs after this gate, so
        // read-only candidate admission defers only the adopted-identity record
        // to that hook. Live/physical identity, writer-fence startup state and
        // full recovery admission still apply; startup and certify stay strict.
        let pending = matches!(phase, "preflight" | "activate")
            && !config["switch_adoption_identifier"].is_null()
            && !pg_core::path(&database, "state_dir")?
                .join("identity.json")
                .exists();
        if pending {
            pg_core::validate_config(&database)?;
            super::writer_fence::startup(&database)?;
        } else {
            postgres::check(&database)?;
        }
        if phase == "startup" {
            return Ok(json!({}));
        }
        let settings = recovery::policy(&database)?;
        if pending && config["switch_adoption_identifier"] != settings["system_identifier"] {
            return Err(invalid(
                "switch adoption and recovery must name the same authoritative cluster",
            ));
        }
        pg_core::reject_upgrade(&database)?;
        let socket = Path::new(config["socket_dir"].as_str().unwrap_or("/run/postgresql"));
        let port = config
            .get("port")
            .map(|p| {
                p.as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|p| *p > 0)
                    .ok_or_else(|| invalid("invalid PostgreSQL port"))
            })
            .transpose()?
            .unwrap_or(5432);
        pg_core::inspect_live(
            &database,
            string(settings, "system_identifier")?,
            socket,
            port,
        )?;
        if let Some(checks) = config.get("compatibility_checks") {
            for check in checks
                .as_array()
                .ok_or_else(|| invalid("invalid compatibility checks"))?
            {
                if recovery::query(
                    &database,
                    socket,
                    port,
                    string(check, "database")?,
                    string(check, "sql")?,
                )?
                .trim()
                    != "t"
                {
                    return Err(invalid(format!(
                        "candidate schema/recovery compatibility check failed: {}",
                        string(check, "database")?
                    )));
                }
            }
        }
        // Keep the primary, backup mutation and receipt leases until every database
        // inventory query completes; a receipt returned by value is insufficient.
        // Only the certify transition needs record-certified evidence; preflight and
        // activate gate on backup health.
        let verify = matches!(phase, "activate" | "certify");
        let accepted = if phase == "certify" {
            let accepted =
                recovery::evidence_admission(&database, Some(now), verify, Some(socket), port)?;
            recovery::live_check(&database, socket, port, Some(now))?;
            accepted
        } else {
            recovery::backup_admission(&database, Some(now), verify, Some(socket), port)?
        };
        let mut requirements = serde_json::Map::new();
        if let Some(checks) = config.get("corpus_checks") {
            for (name, checks) in checks
                .as_object()
                .ok_or_else(|| invalid("invalid corpus checks"))?
            {
                let mut entries = vec![];
                for check in checks
                    .as_array()
                    .ok_or_else(|| invalid("invalid corpus checks"))?
                {
                    let paths = super::codec::decode_str(&recovery::query_leased(
                        &database,
                        socket,
                        port,
                        string(check, "database")?,
                        string(check, "sql")?,
                        &accepted.fds(),
                    )?)?;
                    for item in paths.as_array().ok_or_else(|| {
                        invalid("database corpus query must return a JSON array of paths")
                    })? {
                        entries.push(super::custody::database_entry(&check["root"], item)?);
                    }
                }
                requirements.insert(name.clone(), json!(entries));
            }
        }
        let _held = accepted;
        Ok(
            json!({"database_snapshot_sha256":digest(&recovery::source_snapshot_path(&database, settings)?)?,"corpus_requirements":requirements}),
        )
    }
}

pub struct WorkerOptions<'a> {
    pub phase: &'a str,
    pub timeout: Duration,
    pub extra: &'a [String],
    pub command_name: &'a str,
    pub as_root: bool,
    pub input: Option<&'a Value>,
}

pub const MAXIMUM_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub fn read_snapshot(deadline: Instant) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| invalid("cutover inspection budget exhausted"))?;
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll and read operate on the worker's live stdin and local buffer.
        let ready = unsafe {
            libc::poll(
                &mut fd,
                1,
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready == 0 {
            continue;
        }
        let mut buffer = [0u8; 65536];
        let count = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count as usize]);
        if bytes.len() > MAXIMUM_SNAPSHOT_BYTES {
            return Err(invalid("namespace snapshot exceeds its size limit"));
        }
    }
    codec::decode_str(
        std::str::from_utf8(&bytes).map_err(|_| invalid("namespace snapshot is not UTF-8"))?,
    )
}

pub fn execute_worker(
    path: &Path,
    name: &str,
    config: &Value,
    options: WorkerOptions<'_>,
) -> Result<Value> {
    let WorkerOptions {
        phase,
        timeout,
        extra,
        command_name,
        as_root,
        input,
    } = options;
    let account = process::account(string(config, "user")?)?;
    let uid = unsafe { libc::geteuid() };
    if uid != 0 && as_root {
        return Err(invalid("prepared transition inspection requires root"));
    }
    if uid != 0 && uid != account.uid {
        return Err(invalid(format!(
            "inspection requires the resource's service user: {}",
            string(config, "user")?
        )));
    }
    let mut argv = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        command_name.into(),
        "--contract".into(),
        path.to_string_lossy().into_owned(),
        "--host".into(),
        string(&durable::read_config_json(path)?, "host")?.into(),
        "--phase".into(),
        phase.into(),
        "--worker".into(),
        name.into(),
    ];
    argv.extend_from_slice(extra);
    let mut spec = process::CommandSpec::new(argv);
    if let Some(input) = input {
        let bytes = serde_json::to_vec(input)?;
        if bytes.len() > MAXIMUM_SNAPSHOT_BYTES {
            return Err(invalid("namespace snapshot exceeds its size limit"));
        }
        spec.input = Some(bytes);
    }
    spec.timeout = timeout;
    if uid == 0 {
        spec.identity = Some(if as_root {
            process::Identity {
                uid: 0,
                gid: 0,
                groups: vec![],
            }
        } else {
            account
        });
    }
    // Workers print only their own bounded error line; nested commands keep
    // their diagnostics suppressed inside the worker.
    spec.capture_stderr = true;
    let output = process::run(&spec)?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason: String = reason
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("harbor-db-cutover: "))
            .unwrap_or("resource inspection failed")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(1024)
            .collect();
        return Err(invalid(reason));
    }
    super::codec::decode(&output.stdout)
}
pub fn check_manifest(
    path: &Path,
    manifest: &Value,
    phase: &str,
    candidate: Option<&str>,
) -> Result<Value> {
    let seconds = manifest[if phase == "preflight" {
        "timeout_seconds"
    } else {
        "activation_timeout_seconds"
    }]
    .as_u64()
    .unwrap_or(if phase == "preflight" { 30 } else { 900 });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let contracts = validate_storage_namespaces_until(manifest, deadline)?;
    let mut results = serde_json::Map::new();
    let mut failures = vec![];
    let resources = manifest["resources"]
        .as_object()
        .ok_or_else(|| invalid("invalid cutover resources"))?;
    for (name, config) in resources {
        let result = (|| {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| invalid("cutover inspection budget exhausted"))?;
            let as_root = prepared_transition(config, phase)?.is_some();
            let mut extra = candidate
                .map(|c| vec!["--candidate".into(), c.into()])
                .unwrap_or_default();
            if contracts.get(name).is_some() {
                extra.push("--authority-contract-stdin".into());
            }
            execute_worker(
                path,
                name,
                config,
                WorkerOptions {
                    phase,
                    timeout: remaining,
                    extra: &extra,
                    command_name: "check",
                    as_root,
                    input: contracts.get(name),
                },
            )
        })();
        match result {
            Ok(v) => {
                results.insert(name.clone(), v);
            }
            Err(e) => failures.push(json!({"resource":name,"reason":e.to_string()})),
        }
    }
    for (name, config) in resources {
        if phase != "startup"
            && let Some(dependency) = config["database_resource"].as_str()
        {
            let source = results.get(name);
            let database = results.get(dependency);
            if source.is_none()
                || database.is_none()
                || source.unwrap()["database_snapshot_sha256"]
                    != database.unwrap()["database_snapshot_sha256"]
                || source
                    .unwrap()
                    .get("database_requirements")
                    .cloned()
                    .unwrap_or_else(|| json!([]))
                    != database.unwrap()["corpus_requirements"]
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| json!([]))
            {
                failures.push(json!({"resource":name,"reason":"database recovery snapshot differs from filesystem custody"}));
            }
        }
    }
    Ok(
        json!({"version":1,"host":manifest["host"],"phase":phase,"status":if failures.is_empty(){"ready"}else{"blocked"},"failures":failures}),
    )
}

pub fn serve(config: &Value, argv: &[String]) -> Result<()> {
    let executable = argv
        .first()
        .filter(|s| Path::new(s).is_absolute())
        .ok_or_else(|| invalid("consumer executable must be an absolute path"))?;
    let (lease, _) = resource::inspection(&config["authority"])?;
    check_resource(config, "startup", None, None)?;
    let mut command = std::process::Command::new(executable);
    command.args(&argv[1..]);
    process::exec(&mut command, &[lease.fd()])
}
