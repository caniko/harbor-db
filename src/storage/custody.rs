//! Independent corpus evidence. This module has no dependency on the cutover dispatcher.
use super::{Result, codec, durable, invalid, process, resource, string};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("invalid {key}")))
}
pub fn paths(v: &Value, key: &str) -> Result<Vec<PathBuf>> {
    array(v, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(PathBuf::from)
                .ok_or_else(|| invalid(format!("invalid {key}")))
        })
        .collect()
}
pub fn name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn unit(s: &str) -> bool {
    s.strip_suffix(".service").is_some_and(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_@.:-".contains(&b))
    })
}
pub fn hex(s: &str, length: usize) -> bool {
    s.len() == length
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn validate_path(s: &str) -> Result<()> {
    if !Path::new(s).is_absolute()
        || s.contains("//")
        || s.ends_with('/') && s != "/"
        || s.split('/').any(|p| p == "." || p == "..")
    {
        return Err(invalid(
            "cutover storage and policy paths must be canonical absolute paths",
        ));
    }
    Ok(())
}
pub fn root_identities(config: &Value) -> Result<Value> {
    Ok(Value::Array(
        paths(&config["authority"], "directories")?
            .iter()
            .map(|p| {
                let m = fs::metadata(p)?;
                Ok(json!({"device":m.dev(),"inode":m.ino(),"mode":m.mode() & 0o7777}))
            })
            .collect::<Result<_>>()?,
    ))
}
fn stable(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn walk(
    root: &Path,
    parent: &Path,
    marker: &Path,
    contents: bool,
    files: &mut serde_json::Map<String, Value>,
) -> Result<()> {
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        if path == marker {
            continue;
        }
        let info = fs::symlink_metadata(&path)?;
        if !info.is_dir() && !info.is_file() {
            return Err(invalid(format!(
                "redirected or special corpus entry: {}",
                path.display()
            )));
        }
        let key = path
            .strip_prefix(root)
            .map_err(|_| invalid("corpus escapes root"))?
            .to_str()
            .ok_or_else(|| invalid("corpus path is not UTF-8"))?
            .to_owned();
        let mut value = json!({"directory":info.is_dir(),"mode":info.mode() & 0o7777,"size":if info.is_file(){info.len()}else{0}});
        if info.is_file() {
            if contents {
                value["sha256"] = json!(codec::file_digest(&path)?);
                if !stable(&info, &fs::symlink_metadata(&path)?) {
                    return Err(invalid(format!(
                        "corpus entry changed during inspection: {}",
                        path.display()
                    )));
                }
            } else {
                value["mtime_ns"] =
                    json!(info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128);
                value["ctime_ns"] =
                    json!(info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128);
                value["inode"] = json!(info.ino());
                value["device"] = json!(info.dev());
            }
        }
        files.insert(key, value);
        if info.is_dir() {
            walk(root, &path, marker, contents, files)?;
        }
    }
    Ok(())
}
pub fn inventory(config: &Value, contents: bool) -> Result<Value> {
    let authority = &config["authority"];
    let mut result = vec![];
    for root in paths(authority, "directories")? {
        if !root.is_dir() || fs::canonicalize(&root)? != root {
            return Err(invalid(format!(
                "authoritative source is missing or redirected: {}",
                root.display()
            )));
        }
        let marker = root.join(format!(
            ".harbor-db-{}-identity.json",
            string(authority, "resource")?
        ));
        let mut files = serde_json::Map::new();
        walk(&root, &root, &marker, contents, &mut files)?;
        if !files.values().any(|v| v["directory"] == false) {
            return Err(invalid(format!(
                "authoritative corpus is empty: {}",
                root.display()
            )));
        }
        result.push(Value::Object(files));
    }
    Ok(Value::Array(result))
}
pub fn writer_active(unit: &str) -> Result<bool> {
    let mut spec = process::CommandSpec::new(vec![
        "systemctl".into(),
        "show".into(),
        unit.into(),
        "--property=ActiveState".into(),
        "--value".into(),
    ]);
    spec.timeout = Duration::from_secs(5);
    let output = process::execute(&spec)?;
    Ok(!matches!(
        process::text(&output)?.trim(),
        "inactive" | "failed"
    ))
}
pub fn require_database_paths(
    inventories: &Value,
    requirements: &Value,
    roots: &[PathBuf],
    git_executable: Option<&str>,
) -> Result<()> {
    let inventories = inventories
        .as_array()
        .ok_or_else(|| invalid("invalid inventory"))?;
    for requirement in requirements
        .as_array()
        .ok_or_else(|| invalid("invalid database requirements"))?
    {
        let path = string(requirement, "path")?;
        let root = requirement["root"]
            .as_u64()
            .ok_or_else(|| invalid("invalid database-bound relative corpus path"))?
            as usize;
        let directory = requirement["directory"]
            .as_bool()
            .ok_or_else(|| invalid("invalid database-bound relative corpus path"))?;
        if path.is_empty()
            || Path::new(path).is_absolute()
            || path.contains("//")
            || path.ends_with('/')
            || path.split('/').any(|s| matches!(s, "." | ".."))
            || Path::new(path)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || root >= inventories.len()
        {
            return Err(invalid("invalid database-bound relative corpus path"));
        }
        let entry = &inventories[root][path];
        if entry.is_null()
            || entry["directory"] != directory
            || !directory && entry["size"] == 0 && requirement.get("size") != Some(&json!(0))
        {
            return Err(invalid(format!(
                "database references missing, empty or incompatible corpus entry: {path}"
            )));
        }
        if let Some(size) = requirement.get("size")
            && (size.as_u64().is_none() || directory || entry["size"] != *size)
        {
            return Err(invalid(format!("database corpus size differs: {path}")));
        }
        if let Some(hash) = requirement.get("sha256")
            && (!hash.as_str().is_some_and(|s| hex(s, 64)) || entry["sha256"] != *hash)
        {
            return Err(invalid(format!(
                "database corpus content hash differs: {path}"
            )));
        }
        if let Some(git) = requirement.get("git_repository") {
            if git != true || !directory || roots.len() != inventories.len() {
                return Err(invalid("invalid database Git repository requirement"));
            }
            validate_git_repository(&roots[root].join(path), requirement, git_executable)?;
        }
    }
    Ok(())
}
pub fn validate_git_repository(
    path: &Path,
    requirement: &Value,
    executable: Option<&str>,
) -> Result<()> {
    let executable = executable
        .filter(|s| Path::new(s).is_absolute())
        .ok_or_else(|| invalid("Git integrity requires a declared absolute executable"))?;
    if requirement
        .get("git_has_commits")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(invalid("invalid database Git commit requirement"));
    }
    let pack = path.join("objects/pack");
    if pack.exists() {
        for entry in fs::read_dir(pack)? {
            if entry?.path().extension().is_some_and(|s| s == "promisor") {
                return Err(invalid(
                    "Git integrity rejects partial-clone promisor packs",
                ));
            }
        }
    }
    for relative in [
        "objects/info/alternates",
        "objects/info/http-alternates",
        "shallow",
    ] {
        let marker = path.join(relative);
        if marker.exists() && fs::metadata(marker)?.len() > 0 {
            return Err(invalid(
                "Git integrity cannot depend on external or shallow history",
            ));
        }
    }
    let mut environment: std::collections::BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("GIT_"))
        .collect();
    environment.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    environment.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    let mut spec = process::CommandSpec::new(vec![
        executable.into(),
        "--no-replace-objects".into(),
        format!("--git-dir={}", path.display()),
    ]);
    spec.environment = Some(environment);
    let prefix = spec.argv.clone();
    spec.argv
        .extend(["rev-parse".into(), "--is-bare-repository".into()]);
    let bare = process::run(&spec)?;
    if !bare.status.success() || process::text(&bare.stdout)?.trim() != "true" {
        return Err(invalid("Git integrity requires a valid bare repository"));
    }
    spec.argv = prefix.clone();
    spec.argv.extend([
        "config".into(),
        "--local".into(),
        "--get-regexp".into(),
        r"^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$".into(),
    ]);
    if process::run(&spec)?.status.code() != Some(1) {
        return Err(invalid(
            "Git integrity rejects partial-clone history or unreadable configuration",
        ));
    }
    spec.argv = prefix.clone();
    spec.argv.extend([
        "fsck".into(),
        "--full".into(),
        "--strict".into(),
        "--no-dangling".into(),
    ]);
    if !process::run(&spec)?.status.success() {
        return Err(invalid("Git history integrity failed"));
    }
    if requirement["git_has_commits"] == true {
        spec.argv = prefix;
        spec.argv.extend([
            "rev-parse".into(),
            "--verify".into(),
            "HEAD^{commit}".into(),
        ]);
        if !process::run(&spec)?.status.success() {
            return Err(invalid("Git history integrity failed"));
        }
    }
    Ok(())
}
pub fn certify_filesystem(
    config: &Value,
    restored_roots: &[PathBuf],
    identifier: &str,
    at: Option<i64>,
    database_snapshot: Option<&str>,
    requirements: &Value,
) -> Result<()> {
    let authority = &config["authority"];
    resource::contract(authority)?;
    let roots = paths(authority, "directories")?;
    let state = Path::new(string(authority, "state_dir")?);
    if identifier.is_empty() || identifier.chars().count() > 128 {
        return Err(invalid(
            "a verified nonempty storage identifier is required",
        ));
    }
    if roots.len() != restored_roots.len()
        || restored_roots.iter().any(|r| {
            !r.is_absolute()
                || fs::canonicalize(r).ok().as_ref() != Some(r)
                || roots
                    .iter()
                    .chain(std::iter::once(&state.to_path_buf()))
                    .any(|s| r.starts_with(s) || s.starts_with(r))
        })
    {
        return Err(invalid(
            "restore roots must be independent absolute corpus directories",
        ));
    }
    if !config["database_resource"].is_null() && database_snapshot.is_none_or(str::is_empty) {
        return Err(invalid(
            "filesystem certification needs matching database recovery evidence",
        ));
    }
    let _lease = durable::lock(
        &state.join("lock"),
        false,
        !state.join("identity.json").exists(),
    )?;
    for unit in array(config, "runtime_units")? {
        if writer_active(
            unit.as_str()
                .ok_or_else(|| invalid("invalid writer unit"))?,
        )? {
            return Err(invalid(
                "source writer is active; establish the declared consistency window",
            ));
        }
    }
    let identities = root_identities(config)?;
    let source = inventory(config, true)?;
    require_database_paths(
        &source,
        requirements,
        &roots,
        config["git_executable"].as_str(),
    )?;
    let mut restore = config.clone();
    restore["authority"]["directories"] = json!(restored_roots);
    let restored = inventory(&restore, true)?;
    for (a, b) in roots.iter().zip(restored_roots) {
        if fs::metadata(a)?.mode() & 0o7777 != fs::metadata(b)?.mode() & 0o7777 {
            return Err(invalid("source and restored corpus root modes differ"));
        }
    }
    require_database_paths(
        &restored,
        requirements,
        restored_roots,
        config["git_executable"].as_str(),
    )?;
    if source != restored {
        return Err(invalid("source and restored historical corpus differ"));
    }
    for (index, (a, b)) in roots.iter().zip(restored_roots).enumerate() {
        for (name, item) in source[index]
            .as_object()
            .ok_or_else(|| invalid("invalid corpus"))?
        {
            if item["directory"] == false {
                let x = fs::metadata(a.join(name))?;
                let y = fs::metadata(b.join(name))?;
                if x.dev() == y.dev() && x.ino() == y.ino() {
                    return Err(invalid(
                        "restore corpus aliases a source file; require an independent restore",
                    ));
                }
            }
        }
    }
    let metadata = inventory(config, false)?;
    if inventory(config, true)? != source {
        return Err(invalid("source changed during custody certification"));
    }
    for unit in array(config, "runtime_units")? {
        if writer_active(
            unit.as_str()
                .ok_or_else(|| invalid("invalid writer unit"))?,
        )? {
            return Err(invalid(
                "source writer changed during custody certification",
            ));
        }
    }
    if root_identities(config)? != identities {
        return Err(invalid(
            "storage identity changed during custody certification",
        ));
    }
    // Publish under the already held exclusive inode lease; never reacquire it.
    let expected = resource::contract(authority)?;
    let identity_path = state.join("identity.json");
    let marker = json!({"resource":authority["resource"],"identity":identifier});
    if identity_path.exists() {
        if resource::verify(authority, &expected)?["identity"] != identifier {
            return Err(invalid("cannot replace adopted storage identity"));
        }
    } else {
        for root in paths(&expected, "directories")? {
            let path = root.join(format!(
                ".harbor-db-{}-identity.json",
                string(authority, "resource")?
            ));
            if path.exists() && durable::read_json(&path)? != marker {
                return Err(invalid("existing storage identity differs"));
            }
        }
        for root in paths(&expected, "directories")? {
            durable::write_json(
                &root.join(format!(
                    ".harbor-db-{}-identity.json",
                    string(authority, "resource")?
                )),
                &marker,
            )?;
        }
        let mut record = expected;
        record["identity"] = json!(identifier);
        durable::write_json(&identity_path, &record)?;
    }
    durable::write_json(
        Path::new(string(config, "custody_file")?),
        &json!({"version":1,"resource":authority["resource"],"identity":identifier,"binding":authority["binding"],"directories":authority["directories"],"completed_at":at.unwrap_or_else(now),"database_snapshot_sha256":database_snapshot,"database_requirements":requirements,"inventory":source,"metadata":metadata,"root_identities":identities}),
    )
}
