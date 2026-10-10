//! Explicit persistent writer exclusion. No failure path thaws or starts a server.
use super::{Result, codec, durable, invalid, pg_core, process, string};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
pub(crate) fn major(config: &Value) -> Result<String> {
    let value = config
        .get("major")
        .ok_or_else(|| invalid("missing major"))?;
    if let Some(text) = value.as_str() {
        Ok(text.to_owned())
    } else if value.is_number() {
        Ok(value.to_string())
    } else {
        Err(invalid("invalid major"))
    }
}
pub(crate) fn path(config: &Value, key: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(string(config, key)?))
}
pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn valid_identifier(id: &str) -> bool {
    id.as_bytes()
        .first()
        .is_some_and(|b| (b'1'..=b'9').contains(b))
        && id.bytes().all(|b| b.is_ascii_digit())
}
pub(crate) fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(crate) fn owned(path: &Path, directory: bool, mask: u32) -> Result<()> {
    let info = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions or side effects.
    if (if directory {
        !info.is_dir()
    } else {
        !info.is_file()
    }) || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & mask != 0
    {
        return Err(invalid(
            "storage must be owned regular non-writable storage",
        ));
    }
    Ok(())
}
fn read_owned(path: &Path, mask: u32) -> Result<Vec<u8>> {
    let mut file = durable::open_regular(path, false)?;
    let info = file.metadata()?;
    if info.uid() != unsafe { libc::geteuid() } || info.mode() & mask != 0 {
        return Err(invalid(
            "writer fence artifacts must be private owned regular files",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn read_private(path: &Path) -> Result<Vec<u8>> {
    read_owned(path, 0o077)
}
fn read_auto(config: &Value) -> Result<Vec<u8>> {
    read_owned(
        &path(config, "data_dir")?.join("postgresql.auto.conf"),
        0o022,
    )
}
pub fn policy(config: &Value) -> Result<Value> {
    let empty = json!({});
    let settings = config.get("writer_fence").unwrap_or(&empty);
    if !settings.is_object() {
        return Err(invalid("invalid writer fence policy"));
    }
    let control = settings
        .get("control_role")
        .unwrap_or(&Value::Null)
        .as_str()
        .unwrap_or(if settings.get("control_role").is_none() {
            "postgres"
        } else {
            ""
        });
    let empty_roles = Vec::new();
    let roles = match settings.get("replication_roles") {
        None => &empty_roles,
        Some(v) => v
            .as_array()
            .ok_or_else(|| invalid("invalid writer fence replication role list"))?,
    };
    let valid_role = |role: &str| {
        role.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_')
            && role
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    if !valid_role(control) {
        return Err(invalid("invalid writer fence role"));
    }
    let mut sorted = BTreeSet::new();
    for role in roles {
        let role = role
            .as_str()
            .ok_or_else(|| invalid("invalid writer fence role"))?;
        if !valid_role(role) || !sorted.insert(role) {
            return Err(invalid("invalid writer fence replication role list"));
        }
    }
    let mut libraries = BTreeSet::new();
    if let Some(values) = settings.get("allowed_preload_libraries") {
        for library in values
            .as_array()
            .ok_or_else(|| invalid("invalid writer fence preload library policy"))?
        {
            let library = library
                .as_str()
                .ok_or_else(|| invalid("invalid writer fence preload library policy"))?;
            if library.is_empty()
                || !library
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            {
                return Err(invalid("invalid writer fence preload library policy"));
            }
            libraries.insert(library);
        }
    }
    Ok(
        json!({"control_role":control,"replication_roles":sorted,"allowed_preload_libraries":libraries}),
    )
}
pub fn hba_contents(settings: &Value) -> Result<Vec<u8>> {
    let control = string(settings, "control_role")?;
    let mut lines = vec![
        "# Harbor DB writer fence: SQL control is local OS-peer only.".to_owned(),
        format!("local all \"{control}\" peer"),
        format!("local replication \"{control}\" peer"),
        "local all all reject".into(),
        "host all all 0.0.0.0/0 reject".into(),
        "host all all ::/0 reject".into(),
    ];
    let roles = settings["replication_roles"]
        .as_array()
        .ok_or_else(|| invalid("invalid replication roles"))?;
    if !roles.is_empty() {
        let roles = roles
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| format!("\"{s}\""))
                    .ok_or_else(|| invalid("invalid role"))
            })
            .collect::<Result<Vec<_>>>()?
            .join(",");
        lines.push(format!(
            "host replication {roles} 127.0.0.1/32 scram-sha-256"
        ));
        lines.push(format!("host replication {roles} ::1/128 scram-sha-256"));
    }
    lines.extend([
        "local replication all reject".into(),
        "host replication all 0.0.0.0/0 reject".into(),
        "host replication all ::/0 reject".into(),
    ]);
    Ok((lines.join("\n") + "\n").into_bytes())
}
pub fn marker(config: &Value) -> Result<PathBuf> {
    Ok(path(config, "state_dir")?.join("writer-fence.json"))
}
fn journal(
    config: &Value,
    selected: Option<&Path>,
    leases: &[std::os::fd::RawFd],
) -> Result<Value> {
    let record: Value = serde_json::from_slice(&read_private(
        &selected.map(Path::to_path_buf).unwrap_or(marker(config)?),
    )?)?;
    let token = string(&record, "token")?;
    if !valid_token(token) {
        return Err(invalid("invalid writer fence token"));
    }
    let history = path(config, "state_dir")?.join("writer-fences");
    let directory = history.join(token);
    owned(&history, true, 0o077)?;
    owned(&directory, true, 0o077)?;
    let expected = json!({"version":1,"resource":config["resource"],"data_dir":config["data_dir"],"major":major(config)?,"policy":policy(config)?,"hba_file":directory.join("pg_hba.conf"),"original_file":directory.join("original-auto.conf"),"selected_file":directory.join("fenced-auto.conf")});
    if expected
        .as_object()
        .unwrap()
        .iter()
        .any(|(k, v)| record.get(k) != Some(v))
    {
        return Err(invalid("writer fence cluster or policy binding changed"));
    }
    for (file, hash) in [
        ("original_file", "original_sha256"),
        ("selected_file", "selected_sha256"),
        ("hba_file", "hba_sha256"),
    ] {
        if codec::digest(&read_private(&path(&record, file)?)?) != string(&record, hash)? {
            return Err(invalid("writer fence retained artifact changed"));
        }
    }
    if read_private(&path(&record, "hba_file")?)? != hba_contents(&record["policy"])? {
        return Err(invalid("writer fence HBA contract changed"));
    }
    if pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        leases,
    )? != string(&record, "system_identifier")?
    {
        return Err(invalid("writer fence cluster identifier changed"));
    }
    if !matches!(record["phase"].as_str(), Some("prepared" | "closing")) {
        return Err(invalid("unknown writer fence phase"));
    }
    Ok(record)
}
pub fn startup(config: &Value) -> Result<Option<Value>> {
    startup_leased(config, &[])
}
pub fn startup_leased(config: &Value, leases: &[std::os::fd::RawFd]) -> Result<Option<Value>> {
    if !exists(&marker(config)?) {
        let auto = path(config, "data_dir")?.join("postgresql.auto.conf");
        if exists(&auto)
            && read_auto(config)?
                .windows(b"# harbor-db-writer-fence=".len())
                .any(|w| w == b"# harbor-db-writer-fence=")
        {
            return Err(invalid("writer fence selector has no journal"));
        }
        return Ok(None);
    }
    let record = journal(config, None, leases)?;
    if record["phase"] != "prepared" {
        return Err(invalid(
            "unfinished writer fence thaw; startup is forbidden",
        ));
    }
    if codec::digest(&read_auto(config)?) != string(&record, "selected_sha256")? {
        return Err(invalid(
            "writer fence startup selector differs; resume offline preparation",
        ));
    }
    Ok(Some(record))
}
fn offline(config: &Value) -> Result<Vec<durable::Lease>> {
    pg_core::validate_config(config)?;
    let state = path(config, "state_dir")?;
    owned(&state, true, 0o022)?;
    let mut leases = vec![durable::lock(
        &state.join("writer-fence.lock"),
        false,
        true,
    )?];
    if exists(&state.join("identity.json")) {
        leases.push(durable::lock(&state.join("lock"), false, false)?);
        pg_core::verify_identity_leased(
            config,
            &leases.iter().map(durable::Lease::fd).collect::<Vec<_>>(),
        )?;
    }
    pg_core::reject_upgrade(config)?;
    pg_core::require_stopped_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &leases.iter().map(durable::Lease::fd).collect::<Vec<_>>(),
    )?;
    Ok(leases)
}
fn prepared(record: &Value) -> Value {
    json!({"status":"prepared-offline","token":record["token"],"system_identifier":record["system_identifier"],"restart_required":true})
}
pub fn open_fence(config: &Value, expected_identifier: &str) -> Result<Value> {
    let held = offline(config)?;
    let leases = held.iter().map(durable::Lease::fd).collect::<Vec<_>>();
    let observed = pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        &leases,
    )?;
    if !valid_identifier(expected_identifier) || observed != expected_identifier {
        return Err(invalid("independently supplied fence identifier differs"));
    }
    let record = if exists(&marker(config)?) {
        let record = journal(config, None, &leases)?;
        if record["phase"] != "prepared" {
            return Err(invalid(
                "resume explicit writer fence thaw before another preparation",
            ));
        }
        let current = codec::digest(&read_auto(config)?);
        if current != string(&record, "original_sha256")?
            && current != string(&record, "selected_sha256")?
        {
            return Err(invalid(
                "automatic configuration changed; retain writer fence",
            ));
        }
        record
    } else {
        let original = read_auto(config)?;
        if original
            .windows(b"# harbor-db-writer-fence=".len())
            .any(|w| w == b"# harbor-db-writer-fence=")
        {
            let tokens = original
                .split(|b| *b == b'\n')
                .filter_map(|line| {
                    line.strip_prefix(b"# harbor-db-writer-fence=")
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .filter(|s| valid_token(s))
                })
                .collect::<Vec<_>>();
            if tokens.len() != 1 {
                return Err(invalid(
                    "unbound writer fence selector; retain stopped primary",
                ));
            }
            let retained = path(config, "state_dir")?
                .join("writer-fences")
                .join(tokens[0])
                .join("prepared.json");
            let record = journal(config, Some(&retained), &leases)?;
            if record["phase"] != "prepared"
                || codec::digest(&original) != string(&record, "selected_sha256")?
            {
                return Err(invalid(
                    "interrupted writer fence selector changed; retain stopped primary",
                ));
            }
            durable::write_json(&marker(config)?, &record)?;
            startup_leased(config, &leases)?;
            return Ok(prepared(&record));
        }
        let settings = policy(config)?;
        let token = token()?;
        let state = path(config, "state_dir")?;
        let history = state.join("writer-fences");
        if !exists(&history) {
            fs::DirBuilder::new().mode(0o700).create(&history)?;
        }
        owned(&history, true, 0o077)?;
        durable::sync_directory(&state)?;
        let directory = history.join(&token);
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        durable::sync_directory(&history)?;
        let hba = directory.join("pg_hba.conf");
        if hba.to_string_lossy().contains(['\'', '\n', '\r', '\\']) {
            return Err(invalid(
                "writer fence storage cannot be represented in PostgreSQL configuration",
            ));
        }
        let mut selected = original.clone();
        selected.extend_from_slice(
            format!(
                "\n# harbor-db-writer-fence={token}\nhba_file = '{}'\n",
                hba.display()
            )
            .as_bytes(),
        );
        let contents = hba_contents(&settings)?;
        let record = json!({"version":1,"phase":"prepared","token":token,"resource":config["resource"],"data_dir":config["data_dir"],"major":major(config)?,"system_identifier":observed,"policy":settings,"hba_file":hba,"original_file":directory.join("original-auto.conf"),"selected_file":directory.join("fenced-auto.conf"),"original_sha256":codec::digest(&original),"selected_sha256":codec::digest(&selected),"hba_sha256":codec::digest(&contents)});
        durable::atomic_write(&hba, &contents)?;
        durable::atomic_write(&path(&record, "original_file")?, &original)?;
        durable::atomic_write(&path(&record, "selected_file")?, &selected)?;
        durable::write_json(&directory.join("prepared.json"), &record)?;
        record
    };
    // Selector is the freeze commit point, before active journal publication.
    durable::atomic_write(
        &path(config, "data_dir")?.join("postgresql.auto.conf"),
        &read_private(&path(&record, "selected_file")?)?,
    )?;
    durable::write_json(&marker(config)?, &record)?;
    startup_leased(config, &leases)?;
    Ok(prepared(&record))
}
pub fn close_fence(config: &Value, token: &str) -> Result<Value> {
    let held = offline(config)?;
    let leases = held.iter().map(durable::Lease::fd).collect::<Vec<_>>();
    let mut record = journal(config, None, &leases)?;
    if record["token"] != token {
        return Err(invalid("writer fence thaw token differs"));
    }
    let current = codec::digest(&read_auto(config)?);
    if current != string(&record, "selected_sha256")?
        && !(record["phase"] == "closing" && current == string(&record, "original_sha256")?)
    {
        return Err(invalid(
            "automatic configuration changed; retain writer fence",
        ));
    }
    record["phase"] = json!("closing");
    durable::write_json(&marker(config)?, &record)?;
    durable::atomic_write(
        &path(config, "data_dir")?.join("postgresql.auto.conf"),
        &read_private(&path(&record, "original_file")?)?,
    )?;
    let receipt = path(&record, "hba_file")?
        .parent()
        .unwrap()
        .join("closed.json");
    durable::write_json(&receipt, &closed_receipt(&record, token))?;
    fs::remove_file(marker(config)?)?;
    durable::sync_directory(&path(config, "state_dir")?)?;
    Ok(json!({"status":"closed-offline","token":token,"receipt":receipt,"restart_required":true}))
}
fn closed_receipt(record: &Value, token: &str) -> Value {
    json!({"status":"closed-offline","token":token,"system_identifier":record["system_identifier"],"original_sha256":record["original_sha256"]})
}
pub fn inspect_offline(config: &Value, token: &str, phase: &str) -> Result<Value> {
    pg_core::validate_config(config)?;
    let lease = durable::lock(
        &path(config, "state_dir")?.join("writer-fence.lock"),
        true,
        false,
    )?;
    let leases = [lease.fd()];
    pg_core::reject_upgrade(config)?;
    pg_core::require_stopped_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &leases,
    )?;
    let record = match phase {
        "prepared" => {
            let record = startup_leased(config, &leases)?
                .ok_or_else(|| invalid("offline writer fence token differs"))?;
            if record["token"] != token {
                return Err(invalid("offline writer fence token differs"));
            }
            record
        }
        "closed" => {
            if exists(&marker(config)?) || !valid_token(token) {
                return Err(invalid("unfinished writer fence thaw"));
            }
            let directory = path(config, "state_dir")?.join("writer-fences").join(token);
            let record = journal(config, Some(&directory.join("prepared.json")), &leases)?;
            let receipt: Value =
                serde_json::from_slice(&read_private(&directory.join("closed.json"))?)?;
            if receipt != closed_receipt(&record, token)
                || codec::digest(&read_auto(config)?) != string(&record, "original_sha256")?
            {
                return Err(invalid("writer fence thaw boundary changed"));
            }
            record
        }
        _ => return Err(invalid("unknown offline writer fence phase")),
    };
    Ok(
        json!({"status":format!("{phase}-offline"),"token":token,"resource":config["resource"],"data_dir":config["data_dir"],"major":major(config)?,"system_identifier":record["system_identifier"]}),
    )
}
pub fn inspect_live(config: &Value, token: &str, socket: &Path, port: u16) -> Result<Value> {
    inspect_live_leased(config, token, socket, port, &[])
}
pub fn inspect_live_leased(
    config: &Value,
    token: &str,
    socket: &Path,
    port: u16,
    outer: &[std::os::fd::RawFd],
) -> Result<Value> {
    pg_core::validate_config(config)?;
    if !socket.is_absolute() || socket.to_string_lossy().contains(',') || port == 0 {
        return Err(invalid(
            "writer fence inspection needs a local socket and port",
        ));
    }
    let lease = durable::lock(
        &path(config, "state_dir")?.join("writer-fence.lock"),
        true,
        false,
    )?;
    let mut leases = outer.to_vec();
    leases.push(lease.fd());
    let record = startup_leased(config, &leases)?
        .ok_or_else(|| invalid("writer fence inspection token differs"))?;
    if record["token"] != token {
        return Err(invalid("writer fence inspection token differs"));
    }
    let sql = "SELECT json_build_object('data_dir', current_setting('data_directory'), 'major', (current_setting('server_version_num')::int / 10000)::text, 'system_identifier', system_identifier::text, 'hba_file', current_setting('hba_file'), 'control_role', current_user, 'in_recovery', pg_is_in_recovery(), 'fsync', current_setting('fsync'), 'full_page_writes', current_setting('full_page_writes'), 'synchronous_commit', current_setting('synchronous_commit'), 'preload_libraries', CASE WHEN current_setting('shared_preload_libraries') = '' THEN '[]'::json ELSE to_json(string_to_array(current_setting('shared_preload_libraries'), ',')) END, 'logical_subscriptions', (SELECT count(*) FROM pg_subscription WHERE subenabled), 'prepared_transactions', (SELECT count(*) FROM pg_prepared_xacts), 'other_writers', (SELECT count(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND backend_type NOT IN ('autovacuum launcher', 'autovacuum worker', 'background writer', 'checkpointer', 'walwriter', 'walsender', 'archiver', 'logical replication launcher', 'io worker'))) FROM pg_control_system();";
    let mut spec = process::CommandSpec::new(vec![
        path(config, "package")?
            .join("bin/psql")
            .to_string_lossy()
            .into_owned(),
        "-X".into(),
        "-w".into(),
        "-A".into(),
        "-t".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "-h".into(),
        socket.to_string_lossy().into_owned(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        string(&record["policy"], "control_role")?.into(),
        "-d".into(),
        "postgres".into(),
        "-c".into(),
        sql.into(),
    ]);
    let mut env = std::env::vars()
        .filter(|(k, _)| !k.starts_with("PG"))
        .collect::<std::collections::BTreeMap<_, _>>();
    env.insert("PGCONNECT_TIMEOUT".into(), "5".into());
    env.insert(
        "PGOPTIONS".into(),
        "-c statement_timeout=10000 -c default_transaction_read_only=on".into(),
    );
    spec.environment = Some(env);
    spec.timeout = Duration::from_secs(15);
    spec.leases = leases;
    let mut observed: Value = serde_json::from_slice(&process::execute(&spec)?)?;
    if let Some(libraries) = observed["preload_libraries"].as_array() {
        let mut values = libraries
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_owned())
                    .ok_or_else(|| invalid("invalid preload library"))
            })
            .collect::<Result<Vec<_>>>()?;
        values.sort();
        observed["preload_libraries"] = json!(values);
    }
    let expected = json!({"data_dir":config["data_dir"],"major":major(config)?,"system_identifier":record["system_identifier"],"hba_file":record["hba_file"],"control_role":record["policy"]["control_role"],"in_recovery":false,"fsync":"on","full_page_writes":"on","synchronous_commit":"on","preload_libraries":record["policy"]["allowed_preload_libraries"],"logical_subscriptions":0,"prepared_transactions":0,"other_writers":0});
    if observed != expected {
        return Err(invalid(
            "live writer fence is not exclusive at the bound primary",
        ));
    }
    Ok(
        json!({"status":"ready","token":token,"system_identifier":record["system_identifier"],"hba_sha256":record["hba_sha256"]}),
    )
}
