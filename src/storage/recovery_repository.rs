//! Additive source-local capture selection. Callers retain the repository lease.
use super::{Result, codec, durable, invalid, recovery, string, writer_fence};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn source_local(settings: &Value) -> Result<bool> {
    match settings.get("repository_protocol") {
        None => Ok(false),
        Some(Value::String(value)) if value == "legacy" => Ok(false),
        Some(Value::String(value)) if value == "source-local-v1" => Ok(true),
        _ => Err(invalid("invalid recovery repository protocol")),
    }
}

pub fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 128
        && identifier.as_bytes()[0].is_ascii_alphanumeric()
        && identifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && !identifier.ends_with(".partial")
}

fn bounded_carrier(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    durable::open_regular(&recovery::absolute(path)?, false)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(
            "recovery repository carrier exceeds its byte limit",
        ));
    }
    Ok(bytes)
}

/// Frozen decoded metadata and byte-derived binding; no publication authority.
pub struct SelectedCapture {
    directory: PathBuf,
    metadata_path: PathBuf,
    snapshot_path: PathBuf,
    metadata: Value,
    binding: Value,
}
impl SelectedCapture {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }
    pub fn metadata(&self) -> &Value {
        &self.metadata
    }
    pub fn binding(&self) -> &Value {
        &self.binding
    }
    pub fn validate_snapshot(&self, token: Option<&str>, records: &Value) -> Result<()> {
        if token != self.metadata["writer_fence_token"].as_str()
            || records != &self.metadata["record_hashes"]
        {
            return Err(invalid(
                "source-local capture differs from the live fenced records",
            ));
        }
        Ok(())
    }
}

pub fn select(config: &Value, settings: &Value, now: i64) -> Result<SelectedCapture> {
    if !source_local(settings)? {
        return Err(invalid(
            "source-local capture selection requires source-local-v1",
        ));
    }
    let root = recovery::absolute(Path::new(string(settings, "backup_root")?))?;
    let protocol = bounded_carrier(&root.join("recovery/PROTOCOL"), 256)?;
    if std::str::from_utf8(&protocol)
        .map_err(|_| invalid("invalid recovery repository protocol"))?
        .trim()
        != "source-local-v1"
    {
        return Err(invalid("invalid recovery repository protocol"));
    }
    let selector = recovery::absolute(&root.join("recovery/SELECTED"))?;
    let mut bytes = Vec::new();
    durable::open_regular(&selector, false)?
        .take(257)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 {
        return Err(invalid("capture selector exceeds 256 bytes"));
    }
    let identifier = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("invalid capture identifier"))?
        .trim();
    if !valid_identifier(identifier) {
        return Err(invalid("invalid capture identifier"));
    }
    let metadata_path = recovery::absolute(
        &root
            .join("recovery/captures")
            .join(format!("{identifier}.json")),
    )?;
    let snapshots = recovery::absolute(&root.join("recovery/snapshots"))?;
    if !std::fs::metadata(&snapshots)?.is_dir() {
        return Err(invalid("capture snapshots namespace is not a directory"));
    }
    let snapshot_path = recovery::absolute(&snapshots.join(format!("{identifier}.json")))?;
    // Decode and hash the same opened immutable carrier, rather than reopening it.
    let metadata_bytes = bounded_carrier(&metadata_path, 16 << 20)?;
    let pin_bytes = bounded_carrier(
        &root
            .join("recovery/pins")
            .join(format!("{identifier}.json")),
        16 << 20,
    )?;
    if pin_bytes != metadata_bytes {
        return Err(invalid(
            "capture metadata differs from its immutable retention pin",
        ));
    }
    let meta = super::codec::decode(&metadata_bytes)?;
    let backup_id = string(&meta, "backup_id")?;
    if !valid_identifier(backup_id) {
        return Err(invalid("invalid completed backup identifier"));
    }
    let directory = recovery::absolute(&root.join("base").join(backup_id))?;
    let manifest = recovery::absolute(&directory.join("backup_manifest"))?;
    let (manifest_value, manifest_digest) = super::backup_manifest::read(&manifest)?;
    let ranges = manifest_value["WAL-Ranges"]
        .as_array()
        .filter(|ranges| !ranges.is_empty())
        .ok_or_else(|| invalid("source-local capture requires manifest WAL ranges"))?;
    let timeline = meta["timeline"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= u32::MAX.into())
        .ok_or_else(|| invalid("invalid capture timeline"))?;
    let mut stop = 0;
    for range in ranges {
        let start = recovery::lsn(&range["Start-LSN"])?;
        let end = recovery::lsn(&range["End-LSN"])?;
        if range["Timeline"].as_u64() != Some(timeline) || start > end {
            return Err(invalid(
                "capture requires one consistent manifest WAL timeline and valid ranges",
            ));
        }
        stop = stop.max(end);
    }
    if recovery::lsn(&meta["backup_stop_lsn"])? != stop
        || recovery::lsn(&meta["post_backup_lsn"])? <= stop
    {
        return Err(invalid(
            "capture recovery point does not follow the actual manifest stop LSN",
        ));
    }
    let names = settings["record_checks"]
        .as_array()
        .ok_or_else(|| invalid("invalid record checks"))?
        .iter()
        .map(|v| string(v, "name"))
        .collect::<Result<BTreeSet<_>>>()?;
    let hashes = meta["record_hashes"]
        .as_object()
        .ok_or_else(|| invalid("invalid capture record hashes"))?;
    let segment = meta["wal_segment_bytes"]
        .as_u64()
        .filter(|n| (1 << 20..=1 << 30).contains(n) && n.is_power_of_two());
    if meta["version"].as_u64() != Some(1)
        || meta["capture_id"] != identifier
        || meta["manifest_sha256"] != manifest_digest
        || meta["record_contract_sha256"] != recovery::contract(settings)?
        || !meta["writer_fence_token"]
            .as_str()
            .is_some_and(writer_fence::valid_token)
        || meta["epoch_id"] != meta["writer_fence_token"]
        || segment.is_none()
        || names.is_empty()
        || hashes.keys().map(String::as_str).collect::<BTreeSet<_>>() != names
        || hashes.values().any(|v| {
            !v.as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
    {
        return Err(invalid(
            "invalid source-local capture metadata or manifest binding",
        ));
    }
    if writer_fence::major(&json!({"major":meta["pg_major"]}))? != writer_fence::major(config)?
        || meta["system_identifier"] != settings["system_identifier"]
    {
        return Err(invalid(
            "backup identity does not match the declared primary",
        ));
    }
    if recovery::lsn(&meta["post_backup_lsn"])? <= recovery::lsn(&meta["backup_stop_lsn"])? {
        return Err(invalid("backup recovery point must follow its stop LSN"));
    }
    let actual_now: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("invalid system clock"))?
        .as_secs()
        .try_into()
        .map_err(|_| invalid("invalid system clock"))?;
    recovery::fresh(
        &meta["completed_at"],
        actual_now,
        settings["max_age_seconds"]
            .as_i64()
            .ok_or_else(|| invalid("invalid evidence age"))?,
    )?;
    use std::os::unix::fs::MetadataExt;
    recovery::fresh(
        &json!(std::fs::metadata(&manifest)?.mtime()),
        now,
        settings["max_age_seconds"]
            .as_i64()
            .ok_or_else(|| invalid("invalid evidence age"))?,
    )?;
    let binding = json!({"backup_id":backup_id,"system_identifier":settings["system_identifier"],"major":writer_fence::major(config)?,"epoch_id":meta["epoch_id"],"manifest_sha256":manifest_digest,"recovery_target_lsn":meta["post_backup_lsn"],"metadata_sha256":codec::digest(&metadata_bytes)});
    Ok(SelectedCapture {
        directory,
        metadata_path,
        snapshot_path,
        metadata: meta,
        binding,
    })
}
