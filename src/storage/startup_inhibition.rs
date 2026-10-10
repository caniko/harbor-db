//! Root-owned persistent systemd barrier. Release never starts or thaws PostgreSQL.
use super::{Result, durable, invalid, pg_core, process, string, writer_fence};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};
use writer_fence::{exists, major, owned, path};

fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(invalid("persistent startup inhibition requires root"));
    }
    Ok(())
}
fn owned_ancestors(path: &Path) -> Result<()> {
    let root_uid = durable::root_owner_uid()?;
    for parent in path.ancestors().skip(1) {
        if exists(parent) {
            let info = fs::symlink_metadata(parent)?;
            let sticky_root = info.uid() == root_uid && info.mode() & libc::S_ISVTX != 0;
            if !info.is_dir()
                || info.uid() != root_uid
                || (info.mode() & 0o022 != 0 && !sticky_root)
            {
                return Err(invalid(
                    "startup inhibition storage has an untrusted ancestor",
                ));
            }
        }
    }
    Ok(())
}
fn resolved(path: &Path) -> Result<PathBuf> {
    super::recovery::absolute(path)
}
fn settings(config: &Value) -> Result<(&Value, PathBuf, PathBuf)> {
    pg_core::validate_config(config)?;
    let policy = config
        .get("startup_inhibition")
        .filter(|v| v.is_object())
        .ok_or_else(|| invalid("missing startup inhibition policy"))?;
    let mut units = vec![string(policy, "unit")?];
    if let Some(setup) = policy.get("setup_units") {
        for unit in setup
            .as_array()
            .ok_or_else(|| invalid("invalid startup inhibition service units"))?
        {
            units.push(
                unit.as_str()
                    .ok_or_else(|| invalid("invalid startup inhibition service unit"))?,
            );
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for unit in units {
        let name = unit
            .strip_suffix(".service")
            .ok_or_else(|| invalid("invalid startup inhibition service unit"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !seen.insert(unit)
        {
            return Err(invalid("invalid startup inhibition service unit"));
        }
    }
    for key in [
        "state_dir",
        "drop_in_root",
        "systemctl",
        "busctl",
        "runuser",
        "adapter",
    ] {
        let text = string(policy, key)?;
        let selected = Path::new(text);
        if !selected.is_absolute() || text.chars().any(|c| c.is_whitespace() || "\\%".contains(c)) {
            return Err(invalid("unsafe startup inhibition path"));
        }
        if matches!(key, "state_dir" | "drop_in_root") {
            resolved(selected)?;
            owned_ancestors(selected)?;
        }
    }
    let state = path(policy, "state_dir")?;
    for key in ["data_dir", "state_dir"] {
        let protected = path(config, key)?;
        if state.starts_with(&protected) || protected.starts_with(&state) {
            return Err(invalid(
                "startup inhibition must have separate root-owned storage",
            ));
        }
    }
    let drop_in = path(policy, "drop_in_root")?
        .join(format!("{}.d", string(policy, "unit")?))
        .join("zzzz-harbor-db-startup-inhibition.conf");
    Ok((policy, state, drop_in))
}
fn drop_ins(policy: &Value, primary: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut result = vec![(string(policy, "unit")?.to_owned(), primary.to_owned())];
    if let Some(units) = policy["setup_units"].as_array() {
        for unit in units {
            let unit = unit
                .as_str()
                .ok_or_else(|| invalid("invalid startup unit"))?;
            result.push((
                unit.to_owned(),
                path(policy, "drop_in_root")?
                    .join(format!("{unit}.d"))
                    .join(primary.file_name().unwrap()),
            ));
        }
    }
    Ok(result)
}
pub fn content(state: &Path) -> Vec<u8> {
    format!("# Harbor DB: retained across legacy and guarded generations.\n[Unit]\nConditionPathExists=!{}\n",state.join("inhibited.json").display()).into_bytes()
}
fn binding(config: &Value, policy: &Value, identifier: &str) -> Result<Value> {
    Ok(
        json!({"version":1,"resource":config["resource"],"data_dir":config["data_dir"],"major":major(config)?,"system_identifier":identifier,"policy":policy}),
    )
}
fn record(config: &Value, policy: &Value, state: &Path, identifier: &str) -> Result<Value> {
    let selected = state.join("inhibited.json");
    owned(&selected, false, 0o022)?;
    let value = durable::read_json(&selected)?;
    let expected = binding(config, policy, identifier)?;
    if !value.is_object()
        || expected
            .as_object()
            .unwrap()
            .iter()
            .any(|(key, item)| value.get(key) != Some(item))
        || !value["token"]
            .as_str()
            .is_some_and(writer_fence::valid_token)
    {
        return Err(invalid("startup inhibition binding changed"));
    }
    Ok(value)
}
fn command(argv: Vec<String>, leases: &[std::os::fd::RawFd]) -> Result<String> {
    let mut spec = process::CommandSpec::new(argv);
    spec.timeout = Duration::from_secs(30);
    spec.leases = leases.to_vec();
    process::text(&process::execute(&spec)?)
}
/// Parse the shell-quoted systemctl path list without invoking a shell.
fn words(text: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut active = false;
    for c in text.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            active = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            active = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            active = true;
        } else if c.is_whitespace() {
            if active {
                result.push(std::mem::take(&mut word));
                active = false;
            }
        } else {
            word.push(c);
            active = true;
        }
    }
    if escaped || quote.is_some() {
        return Err(invalid("invalid systemd drop-in path list"));
    }
    if active {
        result.push(word);
    }
    Ok(result)
}
pub fn effective_condition(observed: &Value, state: &Path) -> Result<()> {
    let required = json!([
        "ConditionPathExists",
        false,
        true,
        state.join("inhibited.json")
    ]);
    if observed["type"] != "a(sbbsi)"
        || !observed["data"].as_array().is_some_and(|values| {
            values.iter().any(|condition| {
                condition.as_array().is_some_and(|parts| {
                    parts.len() >= 4 && parts[..4] == required.as_array().unwrap()[..]
                })
            })
        })
    {
        return Err(invalid(
            "systemd's effective startup inhibition condition is absent",
        ));
    }
    Ok(())
}
fn loaded(
    policy: &Value,
    drop_in: &Path,
    state: &Path,
    unit: &str,
    leases: &[std::os::fd::RawFd],
) -> Result<()> {
    let output = command(
        vec![
            string(policy, "systemctl")?.into(),
            "show".into(),
            unit.into(),
            "--property=DropInPaths".into(),
            "--value".into(),
        ],
        leases,
    )?;
    if !words(&output)?.iter().any(|p| Path::new(p) == drop_in) {
        return Err(invalid(
            "startup inhibition drop-in is not loaded by systemd",
        ));
    }
    let escaped = unit
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_string()
            } else {
                format!("_{:02x}", c as u32)
            }
        })
        .collect::<String>();
    let output = command(
        vec![
            string(policy, "busctl")?.into(),
            "--json=short".into(),
            "get-property".into(),
            "org.freedesktop.systemd1".into(),
            format!("/org/freedesktop/systemd1/unit/{escaped}"),
            "org.freedesktop.systemd1.Unit".into(),
            "Conditions".into(),
        ],
        leases,
    )?;
    effective_condition(&super::codec::decode_str(&output)?, state)
}
fn check_drop_in(drop_in: &Path, state: &Path) -> Result<()> {
    owned(drop_in, false, 0o022)?;
    let mut file = durable::open_regular(drop_in, false)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    if bytes != content(state) {
        return Err(invalid(
            "startup inhibition drop-in differs; preserve foreign policy",
        ));
    }
    Ok(())
}
fn provision_directory(path: &Path, mode: u32) -> Result<()> {
    let mut missing = Vec::new();
    for parent in path.ancestors() {
        if exists(parent) {
            break;
        }
        missing.push(parent.to_owned());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)?;
    owned(path, true, 0o022)?;
    for parent in missing {
        durable::sync_directory(&parent)?;
        durable::sync_directory(
            parent
                .parent()
                .ok_or_else(|| invalid("directory has no parent"))?,
        )?;
    }
    Ok(())
}
pub fn inhibit(config: &Value, identifier: &str) -> Result<Value> {
    require_root()?;
    let (policy, state, drop_in) = settings(config)?;
    if !writer_fence::valid_identifier(identifier)
        || pg_core::inspect_cluster(
            &path(config, "package")?,
            &path(config, "data_dir")?,
            &major(config)?,
        )? != identifier
    {
        return Err(invalid("startup inhibition identifier differs"));
    }
    provision_directory(&state, 0o700)?;
    let lease = durable::lock(&state.join("lock"), false, true)?;
    let leases = [lease.fd()];
    for (_, barrier) in drop_ins(policy, &drop_in)? {
        let parent = barrier.parent().unwrap();
        provision_directory(parent, 0o755)?;
        owned(parent.parent().unwrap(), true, 0o022)?;
        if exists(&barrier) {
            check_drop_in(&barrier, &state)?;
        } else {
            durable::atomic_write(&barrier, &content(&state))?;
        }
    }
    let held = if exists(&state.join("inhibited.json")) {
        record(config, policy, &state, identifier)?
    } else {
        let mut held = binding(config, policy, identifier)?;
        held["token"] = json!(writer_fence::token()?);
        durable::write_json(&state.join("inhibited.json"), &held)?;
        held
    };
    command(
        vec![string(policy, "systemctl")?.into(), "daemon-reload".into()],
        &leases,
    )?;
    for (unit, barrier) in drop_ins(policy, &drop_in)? {
        loaded(policy, &barrier, &state, &unit, &leases)?;
    }
    Ok(json!({"status":"startup-inhibited","token":held["token"],"unit":policy["unit"]}))
}
pub fn release(
    config: &Value,
    manifest: &Path,
    token: &str,
    fence_token: &str,
    phase: &str,
) -> Result<Value> {
    require_root()?;
    let (policy, state, drop_in) = settings(config)?;
    owned(&state, true, 0o022)?;
    let lease = durable::lock(&state.join("lock"), false, false)?;
    let anchor = path(config, "state_dir")?.join("writer-fence.lock");
    let file = durable::open_regular(&anchor, false)?;
    let info = file.metadata()?;
    if info.uid() != process::account("postgres")?.uid || info.mode() & 0o022 != 0 {
        return Err(invalid(
            "startup release requires the retained service-owned fence anchor",
        ));
    }
    // Acquire the checked inode itself, not a second path lookup.
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let leases = [lease.fd(), file.as_raw_fd()];
    let identifier = pg_core::inspect_cluster_leased(
        &path(config, "package")?,
        &path(config, "data_dir")?,
        &major(config)?,
        &leases,
    )?;
    let held = record(config, policy, &state, &identifier)?;
    if held["token"] != token {
        return Err(invalid("startup inhibition token differs"));
    }
    if !matches!(phase, "prepared" | "closed") {
        return Err(invalid("unknown startup release boundary"));
    }
    for (unit, barrier) in drop_ins(policy, &drop_in)? {
        check_drop_in(&barrier, &state)?;
        loaded(policy, &barrier, &state, &unit, &leases)?;
    }
    let output = command(
        vec![
            string(policy, "runuser")?.into(),
            "-u".into(),
            "postgres".into(),
            "--".into(),
            string(policy, "adapter")?.into(),
            "--config".into(),
            manifest.to_string_lossy().into_owned(),
            "inspect-offline-fence".into(),
            "--token".into(),
            fence_token.into(),
            "--phase".into(),
            phase.into(),
        ],
        &leases,
    )?;
    let boundary = super::codec::decode_str(&output)?;
    let expected = json!({"status":format!("{phase}-offline"),"token":fence_token,"resource":config["resource"],"data_dir":config["data_dir"],"major":major(config)?,"system_identifier":identifier});
    if boundary != expected {
        return Err(invalid(
            "startup release requires the exact bound offline fence boundary",
        ));
    }
    let receipt = state.join(format!("{}.released.json", string(&held, "token")?));
    durable::write_json(&receipt, &json!({"inhibition":held,"boundary":boundary}))?;
    fs::remove_file(state.join("inhibited.json"))?;
    durable::sync_directory(&state)?;
    Ok(json!({"status":"startup-released","token":token,"receipt":receipt}))
}
