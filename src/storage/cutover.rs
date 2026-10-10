//! Mandatory read-only deployment admission and explicit independent custody certification.
pub use super::custody::{
    certify_filesystem, inventory, require_database_paths, root_identities,
    validate_git_repository, validate_path, writer_active,
};
use super::{
    Result, application_transition, codec, custody, durable, invalid, pg_core, postgres, process,
    recovery, resource, string,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::Path,
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
        } else {
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
            validate_path(string(authority, "state_dir")?)?;
            validate_path(string(entry, "custody_file")?)?;
            if Path::new(string(entry, "custody_file")?).parent()
                != Some(Path::new(string(authority, "state_dir")?))
            {
                return Err(invalid(
                    "custody receipt must be directly in its private authority directory",
                ));
            }
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
    Ok(value.clone())
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
    if !matches!(phase, "preflight" | "activate" | "startup" | "certify") {
        return Err(invalid("unsupported cutover phase"));
    }
    let now = at.unwrap_or_else(custody::now);
    if config["kind"] == "filesystem" {
        if let Some(selected) = prepared_transition(config, phase)? {
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
        let metadata = if phase != "startup" {
            Some(inventory(config, false)?)
        } else {
            None
        };
        let authority = &config["authority"];
        let (_lease, adopted) = resource::inspection(authority)?;
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
        postgres::check(&database)?;
        if phase == "startup" {
            return Ok(json!({}));
        }
        let settings = recovery::policy(&database)?;
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
        let accepted = recovery::admission_lease(
            &database,
            Some(now),
            matches!(phase, "activate" | "certify"),
            Some(socket),
            port,
        )?;
        if phase == "certify" {
            recovery::live_check(&database, socket, port, Some(now))?;
        }
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
                        let mut entry = json!({"root":check["root"]});
                        for (key, value) in item
                            .as_object()
                            .ok_or_else(|| invalid("database corpus path must be an object"))?
                        {
                            entry[key] = value.clone();
                        }
                        entries.push(entry);
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
    super::codec::decode(&process::execute(&spec)?)
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
            let extra = candidate
                .map(|c| vec!["--candidate".into(), c.into()])
                .unwrap_or_default();
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
