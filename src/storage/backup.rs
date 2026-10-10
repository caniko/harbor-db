//! Conservative complete-backup retention and timeline-bound WAL floors.
use super::{Result, durable, invalid, retention_json};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, time::SystemTime};

mod pins;

fn modified(path: &Path) -> Result<f64> {
    Ok(fs::metadata(path)?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_else(|error| -error.duration().as_secs_f64()))
}
fn lsn(value: &Value) -> Option<u64> {
    let (high, low) = value.as_str()?.split_once('/')?;
    if [high, low]
        .iter()
        .any(|s| s.is_empty() || s.len() > 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some((u64::from_str_radix(high, 16).ok()? << 32) + u64::from_str_radix(low, 16).ok()?)
}

pub fn prune(
    root: &Path,
    base_days: i64,
    wal_days: i64,
    segment_bytes: u64,
    now: Option<f64>,
) -> Result<()> {
    if !(1024 * 1024..=1024 * 1024 * 1024).contains(&segment_bytes)
        || !segment_bytes.is_power_of_two()
    {
        return Err(invalid("invalid PostgreSQL WAL segment size"));
    }
    let now = match now {
        Some(now) => now,
        None => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| invalid("invalid clock"))?
            .as_secs_f64(),
    };
    let protection = pins::load(root, segment_bytes)?;
    let _lease = durable::backup_lock(&root.join("BACKUP_LOCK"))?;
    let base = root.join("base");
    let mut backups = Vec::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(".partial") {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            return Ok(());
        }
        backups.push((entry.path(), modified(&entry.path())?));
    }
    backups.sort_by(|a, b| b.1.total_cmp(&a.1));
    if backups.is_empty() {
        return Ok(());
    }
    let mut floors = BTreeMap::<u64, u64>::new();
    let mut expired = Vec::new();
    for (index, (path, time)) in backups.iter().enumerate() {
        let retain = index < 2
            || *time >= now - base_days as f64 * 86400.0
            || protection.as_ref().is_some_and(|p| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| p.backups.contains(n))
            });
        let manifest = match retention_json::read(&path.join("backup_manifest")) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let ranges = match manifest.get("WAL-Ranges").and_then(Value::as_array) {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(()),
        };
        for entry in ranges {
            let timeline = match entry.get("Timeline").and_then(Value::as_u64) {
                Some(t) if (1..=0xffff_ffff).contains(&t) => t,
                _ => return Ok(()),
            };
            let (start, end) = match (
                entry.get("Start-LSN").and_then(lsn),
                entry.get("End-LSN").and_then(lsn),
            ) {
                (Some(s), Some(e)) if e >= s => (s, e),
                _ => return Ok(()),
            };
            let _ = end;
            if retain {
                let floor = start / segment_bytes;
                floors
                    .entry(timeline)
                    .and_modify(|v| *v = (*v).min(floor))
                    .or_insert(floor);
            }
        }
        if !retain {
            expired.push(path);
        }
    }
    for path in expired {
        fs::remove_dir_all(path)?;
    }
    durable::sync_directory(&base)?;
    let wal = root.join("wal");
    let mut segments = Vec::new();
    for entry in fs::read_dir(&wal)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 24
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'A'..=b'F'))
            && entry.file_type()?.is_file()
        {
            if entry.metadata()?.len() != segment_bytes {
                return Ok(());
            }
            segments.push((entry.path(), name));
        }
    }
    // Python returns before synchronizing WAL when there are no eligible
    // segments. Base expiration has already been synced; ignored partial
    // transfers and an empty WAL directory have not been mutated.
    if segments.is_empty() {
        return Ok(());
    }
    for (path, name) in segments {
        let timeline =
            u64::from_str_radix(&name[..8], 16).map_err(|_| invalid("invalid WAL timeline"))?;
        let segment = u64::from_str_radix(&name[8..16], 16)
            .map_err(|_| invalid("invalid WAL log"))?
            * ((1u64 << 32) / segment_bytes)
            + u64::from_str_radix(&name[16..], 16).map_err(|_| invalid("invalid WAL segment"))?;
        if floors.get(&timeline).is_some_and(|floor| segment < *floor)
            && modified(&path)? < now - wal_days as f64 * 86400.0
        {
            fs::remove_file(path)?;
        }
    }
    durable::sync_directory(&wal)
}
