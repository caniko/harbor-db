//! PostgreSQL identity inspection, independent of lifecycle orchestration.
use super::{
    Result, codec, durable, invalid,
    process::{self, CommandSpec},
    string,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub fn major(config: &Value) -> Result<String> {
    let value = config
        .get("major")
        .ok_or_else(|| invalid("missing major"))?;
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("invalid PostgreSQL major"));
    }
    Ok(text)
}

/// Resolve existing ancestors as Python Path.resolve does for missing targets.
pub fn unredirected(path: &Path) -> Result<bool> {
    if !path.is_absolute()
        || path
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Ok(false);
    }
    let mut ancestor = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved.as_os_str() == path.as_os_str());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&ancestor).is_ok() {
                    return Ok(false);
                }
                tail.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| invalid("invalid absolute path"))?
                        .to_os_string(),
                );
                ancestor.pop();
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn path(config: &Value, key: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(string(config, key)?))
}

pub fn require_mounts(config: &Value) -> Result<()> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let mounts: Vec<String> = mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(|s| {
            let mut bytes = Vec::new();
            let input = s.as_bytes();
            let mut i = 0;
            while i < input.len() {
                if input[i] == b'\\'
                    && i + 3 < input.len()
                    && input[i + 1..i + 4].iter().all(|b| matches!(b, b'0'..=b'7'))
                {
                    bytes.push(
                        (input[i + 1] - b'0') * 64 + (input[i + 2] - b'0') * 8 + input[i + 3]
                            - b'0',
                    );
                    i += 4;
                } else {
                    bytes.push(input[i]);
                    i += 1;
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect();
    if let Some(required) = config.get("required_mounts") {
        for mount in required
            .as_array()
            .ok_or_else(|| invalid("invalid required_mounts"))?
        {
            let mount = mount
                .as_str()
                .ok_or_else(|| invalid("invalid required mount"))?;
            if !mounts.iter().any(|m| m == mount) {
                return Err(invalid(format!("required mount is absent: {mount}")));
            }
        }
    }
    Ok(())
}

pub fn validate_config(config: &Value) -> Result<()> {
    for key in ["data_dir", "state_dir", "package"] {
        let path = path(config, key)?;
        if !unredirected(&path)? {
            return Err(invalid(format!(
                "{key} must be an absolute non-symlink path: {}",
                path.display()
            )));
        }
    }
    major(config)?;
    if string(config, "resource")?.is_empty() {
        return Err(invalid("resource name is empty"));
    }
    if path(config, "state_dir")?.starts_with(path(config, "data_dir")?) {
        return Err(invalid("state_dir must be outside the cluster"));
    }
    require_mounts(config)
}

pub fn command(argv: Vec<String>, private: bool) -> CommandSpec {
    let mut spec = CommandSpec::new(argv);
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !private || !key.starts_with("PG"))
        .collect();
    env.insert("LC_ALL".into(), "C".into());
    spec.environment = Some(env);
    spec
}

pub fn inspect_cluster(package: &Path, data: &Path, major: &str) -> Result<String> {
    inspect_cluster_leased(package, data, major, &[])
}

pub fn inspect_cluster_leased(
    package: &Path,
    data: &Path,
    major: &str,
    leases: &[std::os::fd::RawFd],
) -> Result<String> {
    let version = fs::read_to_string(data.join("PG_VERSION")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            invalid(format!(
                "cluster is missing at {}; initialization is forbidden",
                data.display()
            ))
        } else {
            e.into()
        }
    })?;
    if fs::symlink_metadata(data)?.file_type().is_symlink() || version.trim() != major {
        return Err(invalid(format!(
            "cluster major mismatch at {}",
            data.display()
        )));
    }
    let mut spec = command(
        vec![
            package.join("bin/pg_controldata").display().to_string(),
            data.display().to_string(),
        ],
        false,
    );
    spec.leases = leases.to_vec();
    let output = process::text(&process::execute(&spec)?)?;
    for line in output.lines() {
        if let Some(id) = line.strip_prefix("Database system identifier:") {
            let id = id.trim();
            if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                return Ok(id.into());
            }
        }
    }
    Err(invalid(format!(
        "cannot inspect cluster identifier at {}",
        data.display()
    )))
}

pub fn identity(config: &Value, id: &str) -> Result<Value> {
    Ok(
        json!({"version":1,"resource":string(config,"resource")?,"data_dir":string(config,"data_dir")?,"major":major(config)?,"system_identifier":id}),
    )
}

pub fn registered(config: &Value) -> Result<Value> {
    let record = durable::read_json(&path(config, "state_dir")?.join("identity.json")).map_err(
        |e| match e {
            super::StorageError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
                invalid("cluster is not adopted; explicit adoption is required")
            }
            _ => e,
        },
    )?;
    if record != identity(config, string(&record, "system_identifier")?)? {
        return Err(invalid(
            "registered cluster identity mismatch (including rollback target)",
        ));
    }
    Ok(record)
}

pub fn verify_identity(config: &Value) -> Result<Value> {
    verify_identity_leased(config, &[])
}

pub fn verify_identity_leased(config: &Value, leases: &[std::os::fd::RawFd]) -> Result<Value> {
    let record = registered(config)?;
    if inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        leases,
    )? != string(&record, "system_identifier")?
    {
        return Err(invalid(
            "cluster identity mismatch: replacement or stale storage",
        ));
    }
    Ok(record)
}

pub fn reject_upgrade(config: &Value) -> Result<()> {
    if path(config, "state_dir")?.join("upgrade.json").exists() {
        return Err(invalid(
            "unfinished upgrade; explicit upgrade resume is required",
        ));
    }
    Ok(())
}

pub fn require_stopped(package: &Path, data: &Path) -> Result<()> {
    require_stopped_leased(package, data, &[])
}

pub fn require_stopped_leased(
    package: &Path,
    data: &Path,
    leases: &[std::os::fd::RawFd],
) -> Result<()> {
    if data.join("postmaster.pid").exists() {
        return Err(invalid(format!(
            "cluster has a postmaster.pid; verify it is stopped: {}",
            data.display()
        )));
    }
    let mut spec = command(
        vec![
            package.join("bin/pg_ctl").display().to_string(),
            "-D".into(),
            data.display().to_string(),
            "status".into(),
        ],
        false,
    );
    spec.leases = leases.to_vec();
    let output = process::run(&spec)?;
    match output.status.code() {
        Some(3) => Ok(()),
        Some(0) => Err(invalid(format!("cluster is running: {}", data.display()))),
        code => Err(invalid(format!(
            "cluster status is indeterminate (code {code:?}): {}",
            data.display()
        ))),
    }
}

pub fn control_digest(data: &Path) -> Result<String> {
    codec::file_digest(&data.join("global/pg_control"))
}

pub fn inspect_live(config: &Value, identifier: &str, socket: &Path, port: u16) -> Result<Value> {
    inspect_live_leased(config, identifier, socket, port, &[])
}

pub fn inspect_live_leased(
    config: &Value,
    identifier: &str,
    socket: &Path,
    port: u16,
    leases: &[std::os::fd::RawFd],
) -> Result<Value> {
    validate_config(config)?;
    if identifier.starts_with('0')
        || identifier.is_empty()
        || !identifier.bytes().all(|b| b.is_ascii_digit())
        || !socket.is_absolute()
        || socket.to_string_lossy().contains(',')
        || port == 0
    {
        return Err(invalid(
            "live inspection requires an identifier and local socket/port",
        ));
    }
    let sql = "SELECT json_build_object('data_dir', current_setting('data_directory'), 'major', (current_setting('server_version_num')::int / 10000)::text, 'system_identifier', system_identifier::text, 'fsync', current_setting('fsync'), 'full_page_writes', current_setting('full_page_writes'), 'synchronous_commit', current_setting('synchronous_commit'), 'in_recovery', pg_is_in_recovery()) FROM pg_control_system();";
    let mut spec = command(
        vec![
            path(config, "package")?
                .join("bin/psql")
                .display()
                .to_string(),
            "--no-psqlrc".into(),
            "--no-password".into(),
            format!("--host={}", socket.display()),
            format!("--port={port}"),
            "--username=postgres".into(),
            "--dbname=postgres".into(),
            "--set=ON_ERROR_STOP=1".into(),
            "--tuples-only".into(),
            "--no-align".into(),
            "--command".into(),
            sql.into(),
        ],
        true,
    );
    spec.timeout = Duration::from_secs(15);
    spec.leases = leases.to_vec();
    spec.environment
        .as_mut()
        .unwrap()
        .insert("PGCONNECT_TIMEOUT".into(), "5".into());
    let observed: Value = serde_json::from_slice(&process::execute(&spec)?)?;
    let expected = json!({"data_dir":string(config,"data_dir")?,"major":major(config)?,"system_identifier":identifier,"fsync":"on","full_page_writes":"on","synchronous_commit":"on","in_recovery":false});
    if observed != expected {
        return Err(invalid(
            "live endpoint differs from the declared durable primary identity",
        ));
    }
    if inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        leases,
    )? != identifier
    {
        return Err(invalid(
            "live endpoint and physical cluster identifiers differ",
        ));
    }
    Ok(observed)
}
