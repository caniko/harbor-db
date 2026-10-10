//! Source-local physical capture. Existing fence authority is retained, never created.
use super::{
    Result, codec, durable, invalid, pg_core, process, recovery, recovery_repository, string,
    writer_fence,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::{fd::RawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SEGMENTS: u64 = 1_000_000;

fn now() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("invalid system clock"))?
        .as_secs()
        .try_into()
        .map_err(|_| invalid("invalid system clock"))
}

fn strict(path: &Path) -> Result<PathBuf> {
    let path = recovery::absolute(path)?;
    for parent in path.ancestors() {
        match fs::symlink_metadata(parent) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("redirected capture namespace"));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn directory(path: &Path) -> Result<PathBuf> {
    let path = strict(path)?;
    if !fs::symlink_metadata(&path)?.is_dir() {
        return Err(invalid("capture namespace requires existing directories"));
    }
    Ok(path)
}

fn bytes(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    durable::open_regular(&strict(path)?, false)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("capture metadata exceeds size limit"));
    }
    Ok(bytes)
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => bytes(path).map(Some),
    }
}

#[derive(Debug)]
struct WalRange {
    timeline: u32,
    start: u64,
    end: u64,
}

fn ranges(manifest: &Value) -> Result<Vec<WalRange>> {
    let items = manifest["WAL-Ranges"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("backup manifest requires actual WAL ranges"))?;
    let mut result = Vec::new();
    for item in items {
        let timeline: u32 = item["Timeline"]
            .as_u64()
            .filter(|n| *n > 0)
            .and_then(|n| n.try_into().ok())
            .ok_or_else(|| invalid("invalid manifest timeline"))?;
        let start = recovery::lsn(&item["Start-LSN"])?;
        let end = recovery::lsn(&item["End-LSN"])?;
        if start >= end {
            return Err(invalid("invalid manifest WAL bounds"));
        }
        if result
            .first()
            .is_some_and(|r: &WalRange| r.timeline != timeline)
        {
            return Err(invalid("multi-timeline backup capture is unsupported"));
        }
        result.push(WalRange {
            timeline,
            start,
            end,
        });
    }
    Ok(result)
}

fn segment_size(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|n| (1 << 20..=1 << 30).contains(n) && n.is_power_of_two())
        .ok_or_else(|| invalid("invalid PostgreSQL WAL segment size"))
}

fn required_segments(ranges: &[WalRange], target: u64, segment: u64) -> Result<(u64, u64)> {
    let first = ranges
        .iter()
        .map(|r| r.start / segment)
        .min()
        .ok_or_else(|| invalid("missing WAL floor"))?;
    let stop = ranges
        .iter()
        .map(|r| r.end)
        .max()
        .ok_or_else(|| invalid("missing backup stop"))?;
    if target <= stop {
        return Err(invalid("capture target must follow actual backup stop LSN"));
    }
    // pg_switch_wal returns the END of its switch record. Recovery's inclusive
    // LSN target compares record START positions, so replay also needs a record
    // in the following segment, not only the just-completed target carrier.
    let last = ((target - 1) / segment)
        .checked_add(1)
        .ok_or_else(|| invalid("invalid replay stop segment"))?;
    let count = last
        .checked_sub(first)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| invalid("invalid WAL segment range"))?;
    if count > MAX_SEGMENTS {
        return Err(invalid("capture WAL range exceeds supported segment limit"));
    }
    Ok((first, last))
}

fn wal_name(timeline: u32, number: u64, segment: u64) -> String {
    let per_log = (1_u64 << 32) / segment;
    format!(
        "{timeline:08X}{:08X}{:08X}",
        number / per_log,
        number % per_log
    )
}

fn wait_wal(
    wal: &Path,
    timeline: u32,
    first: u64,
    last: u64,
    segment: u64,
    wait: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(wait)
        .ok_or_else(|| invalid("invalid WAL wait duration"))?;
    // Advance only over complete carriers. A .partial file never satisfies this check.
    let mut number = first;
    loop {
        let path = wal.join(wal_name(timeline, number, segment));
        let complete = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != segment {
                    return Err(invalid(
                        "receiver WAL carrier is not a complete regular segment",
                    ));
                }
                let file = durable::open_regular(&strict(&path)?, false)?;
                if file.metadata()?.len() != segment {
                    return Err(invalid("receiver WAL segment changed"));
                }
                file.sync_all()?;
                true
            }
        };
        if complete {
            if number == last {
                durable::sync_directory(wal)?;
                return Ok(());
            }
            number += 1;
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(invalid(
                    "missing complete receiver WAL through capture target",
                ));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}

fn query_json(
    config: &Value,
    socket: &Path,
    port: u16,
    sql: &str,
    leases: &[RawFd],
) -> Result<Value> {
    // --quiet suppresses transaction tags; require exactly one JSON document.
    Ok(serde_json::from_str(
        recovery::query_leased(config, socket, port, "postgres", sql, leases)?.trim(),
    )?)
}

fn control_lsn(
    config: &Value,
    socket: &Path,
    port: u16,
    leases: &[RawFd],
    sql: &str,
) -> Result<Value> {
    let mut environment = std::env::vars()
        .filter(|(key, _)| !key.starts_with("PG"))
        .collect::<BTreeMap<_, _>>();
    environment.insert("PGCONNECT_TIMEOUT".into(), "5".into());
    let mut spec = process::CommandSpec::new(vec![
        writer_fence::path(config, "package")?
            .join("bin/psql")
            .display()
            .to_string(),
        "--no-psqlrc".into(),
        "--no-password".into(),
        "--quiet".into(),
        format!("--host={}", socket.display()),
        format!("--port={port}"),
        "--username=postgres".into(),
        "--dbname=postgres".into(),
        "--set=ON_ERROR_STOP=1".into(),
        "--tuples-only".into(),
        "--no-align".into(),
        "--command".into(),
        sql.into(),
    ]);
    spec.environment = Some(environment);
    spec.leases = leases.to_vec();
    spec.timeout = Duration::from_secs(15);
    let observed: Value = serde_json::from_slice(&process::execute(&spec)?)?;
    recovery::lsn(&observed)?;
    Ok(observed)
}

fn switch_wal(config: &Value, socket: &Path, port: u16, leases: &[RawFd]) -> Result<Value> {
    control_lsn(
        config,
        socket,
        port,
        leases,
        "SET default_transaction_read_only=off; SELECT to_json(pg_switch_wal()::text);",
    )
}

fn immutable(root: &Path, path: &Path, content: &[u8], capture_id: &str) -> Result<()> {
    if let Some(existing) = optional_bytes(path)? {
        if existing != content {
            return Err(invalid("immutable capture bytes differ"));
        }
        return Ok(());
    }
    // Scratch is outside pins: the pruner treats every pin entry as authoritative.
    let name = format!("capture-{capture_id}.intent");
    let (scratch, mut file) = durable::temporary_file(root, name.as_ref())?;
    let result = (|| {
        file.write_all(content)?;
        file.sync_all()?;
        durable::publish_file(&scratch, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&scratch);
    }
    result
}

fn publish_generation(
    local: &Path,
    capture_id: &str,
    metadata_bytes: &[u8],
    source_snapshot: &Value,
) -> Result<()> {
    let carrier = strict(&local.join("captures").join(format!("{capture_id}.json")))?;
    immutable(local, &carrier, metadata_bytes, capture_id)?;
    let source = strict(&local.join("snapshots").join(format!("{capture_id}.json")))?;
    let mut source_bytes = codec::encode(source_snapshot, false)?;
    source_bytes.push(b'\n');
    // This single selector commits an immutable metadata/snapshot generation.
    // Failure before commit leaves the previous selected evidence consumable.
    immutable(local, &source, &source_bytes, capture_id)?;
    let selector = strict(&local.join("SELECTED"))?;
    let selection = format!("{capture_id}\n");
    if optional_bytes(&selector)?.as_deref() != Some(selection.as_bytes()) {
        durable::atomic_write(&selector, selection.as_bytes())?;
    }
    Ok(())
}

fn retry_intent(proposed: &Value, content: &[u8], settings: &Value) -> Result<Value> {
    let frozen: Value = serde_json::from_slice(content)?;
    for key in [
        "version",
        "capture_id",
        "backup_id",
        "pg_major",
        "system_identifier",
        "epoch_id",
        "writer_fence_token",
        "manifest_sha256",
        "record_contract_sha256",
        "record_hashes",
        "backup_stop_lsn",
        "wal_segment_bytes",
        "timeline",
    ] {
        if frozen.get(key) != proposed.get(key) {
            return Err(invalid(format!("capture retry differs: {key}")));
        }
    }
    recovery::fresh(
        &frozen["completed_at"],
        now()?,
        settings["max_age_seconds"]
            .as_i64()
            .ok_or_else(|| invalid("invalid evidence age"))?,
    )?;
    recovery::lsn(&frozen["post_backup_lsn"])?;
    Ok(frozen)
}

/// Finalize a verified existing physical backup under a live retained writer fence.
/// A failed WAL wait retains immutable intent; retry requires identical live facts.
pub fn finalize(
    config: &Value,
    backup_id: &str,
    capture_id: &str,
    socket: &Path,
    port: u16,
    wal_wait: Duration,
) -> Result<Value> {
    if !recovery_repository::source_local(&config["recovery"])? {
        return Err(invalid("capture requires source-local-v1"));
    }
    if config["recovery"]["require_writer_fence"] != true {
        return Err(invalid("capture requires the writer fence"));
    }
    if !recovery_repository::valid_identifier(backup_id)
        || !recovery_repository::valid_identifier(capture_id)
    {
        return Err(invalid("invalid capture or backup identifier"));
    }
    let settings = recovery::policy(config)?;
    let socket = directory(socket)?;
    if socket.to_string_lossy().contains(',') || port == 0 {
        return Err(invalid("invalid local capture endpoint"));
    }
    directory(&writer_fence::path(config, "package")?)?;
    let root = directory(&writer_fence::path(settings, "backup_root")?)?;
    let local = directory(&root.join("recovery"))?;
    for name in [
        "base",
        "wal",
        "locks",
        "recovery/pins",
        "recovery/captures",
        "recovery/snapshots",
    ] {
        directory(&root.join(name))?;
    }
    if std::str::from_utf8(&bytes(&local.join("PROTOCOL"))?)
        .map_err(|_| invalid("invalid source-local PROTOCOL encoding"))?
        .trim()
        != "source-local-v1"
    {
        return Err(invalid("invalid source-local PROTOCOL bytes"));
    }
    let snapshot = strict(&writer_fence::path(settings, "snapshot_file")?)?;
    let parent = directory(
        snapshot
            .parent()
            .ok_or_else(|| invalid("invalid snapshot parent"))?,
    )?;
    let fence = recovery::writer_exclusion(config, Some(&socket), port)?;
    let token = string(
        fence
            .binding
            .as_ref()
            .ok_or_else(|| invalid("missing live writer fence binding"))?,
        "token",
    )?;
    let fence_lease = fence
        .lease
        .as_ref()
        .ok_or_else(|| invalid("missing writer fence lease"))?;
    let mutate = durable::lock(&strict(&root.join("locks/mutate"))?, false, false)?;
    let backup_lock = durable::lock(&strict(&root.join("BACKUP_LOCK"))?, false, false)?;
    let evidence = durable::lock(&strict(&parent.join("recovery.lock"))?, false, true)?;
    let leases = [
        fence_lease.fd(),
        mutate.fd(),
        backup_lock.fd(),
        evidence.fd(),
    ];
    pg_core::inspect_live_leased(
        config,
        string(settings, "system_identifier")?,
        &socket,
        port,
        &leases,
    )?;
    let base = directory(&root.join("base").join(backup_id))?;
    if pg_core::inspect_cluster_leased(
        &writer_fence::path(config, "package")?,
        &base,
        &writer_fence::major(config)?,
        &leases,
    )? != string(settings, "system_identifier")?
    {
        return Err(invalid(
            "backup control-file identity differs from the primary",
        ));
    }
    let timeout = Duration::try_from_secs_f64(
        settings["verify_timeout_seconds"]
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or_else(|| invalid("invalid backup verification timeout"))?,
    )
    .map_err(|_| invalid("invalid backup verification timeout"))?;
    let mut verify = process::CommandSpec::new(vec![
        writer_fence::path(config, "package")?
            .join("bin/pg_verifybackup")
            .display()
            .to_string(),
        "--no-parse-wal".into(),
        base.display().to_string(),
    ]);
    verify.timeout = timeout;
    verify.leases = leases.to_vec();
    process::execute(&verify)?;
    let manifest_path = strict(&base.join("backup_manifest"))?;
    let (manifest, manifest_digest) = super::backup_manifest::read(&manifest_path)?;
    let ranges = ranges(&manifest)?;
    let observed = query_json(
        config,
        &socket,
        port,
        "SELECT json_build_object('segment_bytes', pg_size_bytes(current_setting('wal_segment_size')), 'timeline', timeline_id) FROM pg_control_checkpoint()",
        &leases,
    )?;
    let segment = segment_size(&observed["segment_bytes"])?;
    let timeline = ranges[0].timeline;
    if observed["timeline"].as_u64() != Some(u64::from(timeline)) {
        return Err(invalid(
            "backup and primary timeline differ; timeline transitions are unsupported",
        ));
    }
    let captured_at = now()?;
    recovery::fresh(
        &json!(fs::metadata(&manifest_path)?.mtime()),
        captured_at,
        settings["max_age_seconds"]
            .as_i64()
            .ok_or_else(|| invalid("invalid evidence age"))?,
    )?;
    let mut records = serde_json::Map::new();
    for check in settings["record_checks"]
        .as_array()
        .ok_or_else(|| invalid("invalid record checks"))?
    {
        records.insert(
            string(check, "name")?.into(),
            json!(codec::digest(
                recovery::query_leased(
                    config,
                    &socket,
                    port,
                    string(check, "database")?,
                    string(check, "sql")?,
                    &leases
                )?
                .as_bytes()
            )),
        );
    }
    let stop = ranges
        .iter()
        .map(|r| r.end)
        .max()
        .ok_or_else(|| invalid("missing backup stop"))?;
    let stop_text = format!("{:X}/{:X}", stop >> 32, stop & 0xFFFF_FFFF);
    let mut metadata = json!({"version":1,"capture_id":capture_id,"backup_id":backup_id,"pg_major":writer_fence::major(config)?.parse::<u32>().map_err(|_| invalid("invalid major"))?,
        "system_identifier":settings["system_identifier"],"epoch_id":token,"writer_fence_token":token,
        "manifest_sha256":manifest_digest,"record_contract_sha256":recovery::contract(settings)?,
        "record_hashes":records,"backup_stop_lsn":stop_text,"wal_segment_bytes":segment,"timeline":timeline,"completed_at":captured_at});
    let pin = strict(&local.join("pins").join(format!("{capture_id}.json")))?;
    let content = if let Some(content) = optional_bytes(&pin)? {
        metadata = retry_intent(&metadata, &content, settings)?;
        content
    } else {
        // Freeze the ending position in the file this call actually completed.
        // A later flush can advance into a new, still-partial receiver segment.
        metadata["post_backup_lsn"] = switch_wal(config, &socket, port, &leases)?;
        required_segments(
            &ranges,
            recovery::lsn(&metadata["post_backup_lsn"])?,
            segment,
        )?;
        let mut content = codec::encode(&metadata, false)?;
        content.push(b'\n');
        immutable(&local, &pin, &content, capture_id)?;
        content
    };
    let (first, last) = required_segments(
        &ranges,
        recovery::lsn(&metadata["post_backup_lsn"])?,
        segment,
    )?;
    // Emit a genuine post-target recovery stop record and complete its carrier.
    // An accepted repeat already has this segment and needs no further WAL.
    // The immutable target and pin remain those of the first switch, including
    // when reception was interrupted before this second segment was available.
    let stop_segment = root.join("wal").join(wal_name(timeline, last, segment));
    match fs::symlink_metadata(&stop_segment) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let stop = control_lsn(
                config,
                &socket,
                port,
                &leases,
                "SET default_transaction_read_only=off; CHECKPOINT; SELECT to_json(pg_switch_wal()::text);",
            )?;
            if recovery::lsn(&stop)? <= recovery::lsn(&metadata["post_backup_lsn"])? {
                return Err(invalid("replay stop must follow the frozen capture target"));
            }
        }
        Err(error) => return Err(error.into()),
        Ok(_) => (),
    }
    wait_wal(&root.join("wal"), timeline, first, last, segment, wal_wait)?;
    let result = json!({"version":1,"backup_id":backup_id,"system_identifier":settings["system_identifier"],"major":writer_fence::major(config)?,
        "epoch_id":metadata["epoch_id"],"manifest_sha256":metadata["manifest_sha256"],"recovery_target_lsn":metadata["post_backup_lsn"],
        "metadata_sha256":codec::digest(&content),"completed_at":metadata["completed_at"],"record_contract_sha256":metadata["record_contract_sha256"],
        "records":metadata["record_hashes"],"writer_fence_token":token});
    publish_generation(&local, capture_id, &content, &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    type CarrierState = (u64, u64, Vec<u8>);
    type SavedCarrier = (PathBuf, CarrierState);
    use std::os::unix::fs::PermissionsExt;

    fn carrier_state(path: &Path) -> CarrierState {
        let metadata = fs::metadata(path).unwrap();
        (metadata.dev(), metadata.ino(), bytes(path).unwrap())
    }

    // Filesystem protocol fixtures only, not PostgreSQL-produced capture evidence.
    fn generation(id: &str) -> (Vec<u8>, Value) {
        let metadata = json!({"version":1,"capture_id":id,"completed_at":123,
            "record_hashes":{"rows":codec::digest(id.as_bytes())}});
        let mut content = codec::encode(&metadata, false).unwrap();
        content.push(b'\n');
        let source = json!({"version":1,"completed_at":metadata["completed_at"],
            "metadata_sha256":codec::digest(&content),"records":metadata["record_hashes"]});
        (content, source)
    }

    fn previous_generation(local: &Path) -> Vec<SavedCarrier> {
        for name in ["pins", "captures", "snapshots"] {
            fs::create_dir(local.join(name)).unwrap();
        }
        let (content, source) = generation("old");
        immutable(local, &local.join("pins/old.json"), &content, "old").unwrap();
        publish_generation(local, "old", &content, &source).unwrap();
        [
            "pins/old.json",
            "captures/old.json",
            "snapshots/old.json",
            "SELECTED",
        ]
        .into_iter()
        .map(|name| {
            let path = local.join(name);
            let state = carrier_state(&path);
            (path, state)
        })
        .collect()
    }

    fn assert_preserved(states: &[SavedCarrier]) {
        for (path, state) in states {
            assert_eq!(&carrier_state(path), state, "{} changed", path.display());
        }
    }

    #[test]
    fn interrupted_capture_scratch_preserves_its_inode_and_allows_publication() {
        let root = tempfile::tempdir().unwrap();
        let local = fs::canonicalize(root.path()).unwrap();
        let previous = previous_generation(&local);
        let scratch = local.join(format!(".capture-new-{}.intent", std::process::id()));
        fs::write(&scratch, b"interrupted publication must survive").unwrap();
        let saved = carrier_state(&scratch);
        let (content, source) = generation("new");
        publish_generation(&local, "new", &content, &source).unwrap();
        assert_preserved(&previous[..3]);
        assert_eq!(carrier_state(&scratch), saved);
        assert_eq!(bytes(&local.join("SELECTED")).unwrap(), b"new\n");
        let selected = carrier_state(&local.join("snapshots/new.json"));
        publish_generation(&local, "new", &content, &source).unwrap();
        assert_eq!(carrier_state(&local.join("snapshots/new.json")), selected);
        assert_eq!(carrier_state(&scratch), saved);
    }

    #[test]
    fn occupied_snapshot_preserves_previous_generation_then_retry_commits_once() {
        let root = tempfile::tempdir().unwrap();
        let local = fs::canonicalize(root.path()).unwrap();
        let previous = previous_generation(&local);
        let (content, source) = generation("new");
        let pin = local.join("pins/new.json");
        immutable(&local, &pin, &content, "new").unwrap();
        let pin_before = carrier_state(&pin);
        let occupied = local.join("snapshots/new.json");
        fs::create_dir(&occupied).unwrap();
        assert!(publish_generation(&local, "new", &content, &source).is_err());
        assert_preserved(&previous);
        assert_eq!(carrier_state(&pin), pin_before);
        assert!(occupied.is_dir());
        let pending = local.join("captures/new.json");
        let pending_before = carrier_state(&pending);

        // Explicit fixture retry removes only the directory this test created.
        fs::remove_dir(&occupied).unwrap();
        publish_generation(&local, "new", &content, &source).unwrap();
        assert_eq!(bytes(&local.join("SELECTED")).unwrap(), b"new\n");
        assert_eq!(carrier_state(&pending), pending_before);
        assert_eq!(carrier_state(&pin), pin_before);
        let committed = [
            "pins/new.json",
            "captures/new.json",
            "snapshots/new.json",
            "SELECTED",
        ]
        .into_iter()
        .map(|name| {
            let path = local.join(name);
            let state = carrier_state(&path);
            (path, state)
        })
        .collect::<Vec<_>>();
        publish_generation(&local, "new", &content, &source).unwrap();
        assert_preserved(&committed);
        assert_preserved(&previous[..3]);
    }

    #[test]
    fn conflicting_snapshot_is_not_overwritten_or_selected() {
        let root = tempfile::tempdir().unwrap();
        let local = fs::canonicalize(root.path()).unwrap();
        let previous = previous_generation(&local);
        let (content, source) = generation("new");
        let pin = local.join("pins/new.json");
        immutable(&local, &pin, &content, "new").unwrap();
        let pin_before = carrier_state(&pin);
        let conflict = local.join("snapshots/new.json");
        fs::write(&conflict, b"foreign immutable bytes\n").unwrap();
        let conflict_before = carrier_state(&conflict);
        let error = publish_generation(&local, "new", &content, &source).unwrap_err();
        assert!(error.to_string().contains("immutable capture bytes differ"));
        assert_preserved(&previous);
        assert_eq!(carrier_state(&conflict), conflict_before);
        assert_eq!(carrier_state(&pin), pin_before);
        assert!(publish_generation(&local, "new", &content, &source).is_err());
        assert_preserved(&previous);
        assert_eq!(carrier_state(&conflict), conflict_before);
    }

    #[test]
    fn switch_worker_retains_all_four_leases_after_coordinator_descriptors_close() {
        let root = tempfile::tempdir().unwrap();
        let local = fs::canonicalize(root.path()).unwrap();
        let package = local.join("package");
        fs::create_dir_all(package.join("bin")).unwrap();
        let ready = local.join("ready");
        let release = local.join("release");
        let worker = package.join("bin/psql");
        fs::write(&worker, format!(
            "#!/bin/sh\n: > '{}'\nwhile ! test -e '{}'; do sleep 0.01; done\nprintf '\"0/200\"\\n'\n",
            ready.display(), release.display())).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = ["fence.lock", "mutate.lock", "BACKUP_LOCK", "recovery.lock"]
            .map(|name| local.join(name));
        let owned = paths
            .iter()
            .map(|path| durable::lock(path, false, true).unwrap())
            .collect::<Vec<_>>();
        let identities = paths
            .iter()
            .map(|path| carrier_state(path))
            .collect::<Vec<_>>();
        let fds = owned.iter().map(durable::Lease::fd).collect::<Vec<_>>();
        let config = json!({"package":package});
        let socket = local.clone();
        let execution = std::thread::spawn(move || switch_wal(&config, &socket, 5432, &fds));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = ready.exists();
        // Ready is emitted after exec's descriptor handover, so these originals
        // can close without racing the borrowed RawFd input to process::execute.
        let blocked = if started {
            drop(owned);
            paths
                .iter()
                .map(|path| match durable::lock(path, false, false) {
                    Err(super::super::StorageError::Io(error)) => {
                        error.raw_os_error() == Some(libc::EAGAIN)
                    }
                    _ => false,
                })
                .collect::<Vec<_>>()
        } else {
            // Do not invalidate descriptors if the worker has not reached exec.
            fs::write(&release, b"release\n").unwrap();
            let result = execution.join().unwrap();
            drop(owned);
            panic!("switch worker did not reach exec handover: {result:?}");
        };
        // Release before asserting, so a regression cannot strand a test worker.
        fs::write(&release, b"release\n").unwrap();
        let result = execution.join().unwrap().unwrap();
        assert_eq!(result, json!("0/200"));
        assert!(
            blocked.iter().all(|value| *value),
            "worker failed to retain all anchors: {blocked:?}"
        );
        for (path, identity) in paths.iter().zip(identities) {
            let _lease = durable::lock(path, false, false).unwrap();
            assert_eq!(carrier_state(path), identity);
        }
    }

    #[test]
    fn manifest_rejects_empty_reversed_and_multiple_timelines() {
        for value in [
            json!({"WAL-Ranges":[]}),
            json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/20","End-LSN":"0/10"}]}),
            json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/10","End-LSN":"0/20"},{"Timeline":2,"Start-LSN":"0/20","End-LSN":"0/30"}]}),
        ] {
            assert!(ranges(&value).is_err());
        }
    }

    #[test]
    fn target_boundary_requires_previous_segment_and_actual_floor() {
        let segment = 1 << 24;
        let ranges = [WalRange {
            timeline: 1,
            start: segment + 3,
            end: 2 * segment,
        }];
        assert_eq!(
            required_segments(&ranges, 3 * segment, segment).unwrap(),
            (1, 3)
        );
        assert_eq!(
            required_segments(&ranges, 3 * segment + 4, segment).unwrap(),
            (1, 4)
        );
        assert!(required_segments(&ranges, 2 * segment, segment).is_err());
        assert_eq!(wal_name(1, 256, segment), "000000010000000100000000");
        assert!(required_segments(&ranges, (MAX_SEGMENTS + 3) * segment, segment).is_err());
    }

    #[test]
    fn missing_wal_preserves_old_selection_and_pin_then_complete_carrier_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        fs::create_dir(root.join("pins")).unwrap();
        fs::create_dir(root.join("wal")).unwrap();
        fs::write(root.join("SELECTED"), "old\n").unwrap();
        immutable(
            &root,
            &root.join("pins/new.json"),
            b"frozen intent\n",
            "new",
        )
        .unwrap();
        let wal = root.join("wal");
        let name = wal_name(1, 1, 1 << 20);
        fs::File::create(wal.join(format!("{name}.partial")))
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        assert!(wait_wal(&wal, 1, 1, 1, 1 << 20, Duration::ZERO).is_err());
        assert_eq!(bytes(&root.join("SELECTED")).unwrap(), b"old\n");
        assert_eq!(
            bytes(&root.join("pins/new.json")).unwrap(),
            b"frozen intent\n"
        );
        fs::rename(wal.join(format!("{name}.partial")), wal.join(name)).unwrap();
        wait_wal(&wal, 1, 1, 1, 1 << 20, Duration::ZERO).unwrap();
        immutable(
            &root,
            &root.join("pins/new.json"),
            b"frozen intent\n",
            "new",
        )
        .unwrap();
        assert!(immutable(&root, &root.join("pins/new.json"), b"changed", "new").is_err());
    }
}
