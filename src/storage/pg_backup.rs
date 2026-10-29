//! Recovery-ready pg-backup helpers: WAL gap marker, recovery metadata,
//! backup health and memory admission. Every file below `root` is published
//! durably; `root` is the per-source backup directory of `services.harbor-db.pgBackup`.
use super::{Result, durable, invalid, recovery};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn gap_path(root: &Path) -> PathBuf {
    root.join("wal/.gap")
}
fn lsn(text: &str) -> Result<u64> {
    recovery::lsn(&json!(text))
}

// ---------------------------------------------------------------- WAL names

pub fn segment_name(timeline: u32, segment: u64, size: u64) -> String {
    let per = (1u64 << 32) / size;
    format!("{timeline:08X}{:08X}{:08X}", segment / per, segment % per)
}
/// `(timeline, segment number, partial)` of a received WAL file name.
pub fn parse_segment(name: &str, size: u64) -> Option<(u32, u64, bool)> {
    let (stem, partial) = match name.strip_suffix(".partial") {
        Some(stem) => (stem, true),
        None => (name, false),
    };
    if stem.len() != 24
        || !stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
    {
        return None;
    }
    let part = |range: std::ops::Range<usize>| u64::from_str_radix(&stem[range], 16).ok();
    Some((
        u32::try_from(part(0..8)?).ok()?,
        part(8..16)? * ((1u64 << 32) / size) + part(16..24)?,
        partial,
    ))
}
fn wal_segments(root: &Path, size: u64) -> Result<Vec<(u32, u64, bool, PathBuf)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root.join("wal"))? {
        let entry = entry?;
        if let Some((t, s, p)) = parse_segment(&entry.file_name().to_string_lossy(), size) {
            out.push((t, s, p, entry.path()));
        }
    }
    Ok(out)
}
/// WAL range of one published base: `(timeline, first segment, stop LSN)`.
fn manifest_range(tree: &Path, size: u64) -> Result<(u32, u64, u64)> {
    let manifest = durable::read_json(&tree.join("backup_manifest"))?;
    let ranges = manifest["WAL-Ranges"]
        .as_array()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| invalid("backup manifest has no WAL ranges"))?;
    let field = |r: &Value, k: &str| -> Result<u64> { lsn(r[k].as_str().unwrap_or_default()) };
    let timeline = |r: &Value| {
        r["Timeline"]
            .as_u64()
            .and_then(|t| u32::try_from(t).ok())
            .ok_or_else(|| invalid("invalid manifest timeline"))
    };
    let (first, last) = (&ranges[0], &ranges[ranges.len() - 1]);
    Ok((
        timeline(last)?,
        field(first, "Start-LSN")? / size,
        field(last, "End-LSN")?,
    ))
}
/// Every segment `first..=last` of `timeline` is present; only the newest may be partial.
fn contiguous(
    segments: &[(u32, u64, bool, PathBuf)],
    timeline: u32,
    first: u64,
    last: u64,
    allow_partial_last: bool,
) -> Result<()> {
    for segment in first..=last {
        let found = segments.iter().find(|s| {
            s.0 == timeline && s.1 == segment && (!s.2 || (allow_partial_last && segment == last))
        });
        if found.is_none() {
            return Err(invalid(format!(
                "WAL segment {segment} (timeline {timeline}) missing"
            )));
        }
    }
    Ok(())
}

// ----------------------------------------------------- slot state and psql

#[derive(Debug, PartialEq)]
pub enum Slot {
    Missing,
    Lost,
    Present,
}
pub fn parse_slot(output: &str) -> Slot {
    let mut fields = output.trim().split('|');
    let kind = fields.next().unwrap_or_default();
    let restart = fields.next().unwrap_or_default();
    if kind.is_empty() {
        Slot::Missing
    } else if restart.is_empty() {
        Slot::Lost
    } else {
        Slot::Present
    }
}
fn psql(psql: &Path, conninfo: &str, sql: &str) -> Result<String> {
    let output = Command::new(psql)
        .args([
            "-X",
            "-w",
            "-A",
            "-t",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
            conninfo,
        ])
        .output()?;
    if !output.status.success() {
        return Err(invalid(format!(
            "psql failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn slot_state(psql_bin: &Path, conninfo: &str, slot: &str) -> Result<Slot> {
    // READ_REPLICATION_SLOT is a walsender command: it needs only the replication role.
    Ok(parse_slot(&psql(
        psql_bin,
        conninfo,
        &format!("READ_REPLICATION_SLOT {slot}"),
    )?))
}

// ----------------------------------------------------- gap / generation

fn generation(root: &Path) -> Result<String> {
    let path = root.join("GENERATION");
    match fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => Ok(token.trim().to_owned()),
        Ok(_) => Err(invalid("empty GENERATION token")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => rotate(root),
        Err(e) => Err(e.into()),
    }
}
fn rotate(root: &Path) -> Result<String> {
    let mut random = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
    durable::atomic_write(&root.join("GENERATION"), format!("{token}\n").as_bytes())?;
    Ok(token)
}
/// Receiver start-up: a slot that must be recreated while history exists is a WAL gap.
pub fn mark_gap(
    root: &Path,
    segment_bytes: u64,
    psql_bin: &Path,
    conninfo: &str,
    slot: &str,
) -> Result<bool> {
    if slot_state(psql_bin, conninfo, slot)? != Slot::Missing || gap_path(root).exists() {
        return Ok(false);
    }
    let history = fs::read_dir(root.join("wal"))?
        .filter_map(|e| e.ok())
        .any(|e| parse_segment(&e.file_name().to_string_lossy(), segment_bytes).is_some())
        || fs::read_dir(root.join("base"))?.next().is_some();
    if !history {
        return Ok(false);
    }
    let generation = generation(root)?;
    durable::write_json(
        &gap_path(root),
        &json!({"slot": slot, "detected_epoch": now(), "generation": generation}),
    )?;
    Ok(true)
}
/// Base started at `started` and verified: it post-dates the gap, so it closes it.
fn gap_closed_by(root: &Path, started: u64) -> Result<bool> {
    match durable::read_json(&gap_path(root)) {
        Ok(gap) => Ok(gap["detected_epoch"].as_u64().is_some_and(|t| t < started)),
        Err(_) if !gap_path(root).exists() => Ok(false),
        Err(e) => Err(e),
    }
}
fn remove_gap(root: &Path) -> Result<()> {
    fs::remove_file(gap_path(root))?;
    durable::sync_directory(&root.join("wal"))
}
/// Legacy-marker deployments: clear a gap after a verified later base.
pub fn clear_gap(root: &Path, started: u64) -> Result<bool> {
    if !gap_closed_by(root, started)? {
        return Ok(false);
    }
    rotate(root)?;
    remove_gap(root)?;
    Ok(true)
}

// ----------------------------------------------------------- recovery meta

pub struct Finalize<'a> {
    pub root: &'a Path,
    pub id: &'a str,
    pub started: u64,
    pub segment_bytes: u64,
    pub package: &'a Path,
    pub source_host: &'a str,
    pub sql_psql: &'a Path,
    pub sql_conninfo: &'a str,
    pub wal_wait: Duration,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && !id.ends_with(".partial")
}
/// Publish `base/<id>.meta.json`, then `LAST_SUCCESS`, for a verified published base.
pub fn finalize(f: &Finalize) -> Result<Value> {
    if !valid_id(f.id) {
        return Err(invalid("invalid backup identifier"));
    }
    let tree = f.root.join("base").join(f.id);
    let (timeline, first, stop) = manifest_range(&tree, f.segment_bytes)?;
    let major = fs::read_to_string(tree.join("PG_VERSION"))?
        .trim()
        .to_owned();
    let system_identifier = super::pg_core::inspect_cluster(f.package, &tree, &major)?;
    // A restore point plus a WAL switch gives recovery a target past the stop LSN
    // inside a segment that is complete in the archive.
    let label = format!("pgbackup_{}", f.id.replace(['.', '-'], "_"));
    let marker = psql(
        f.sql_psql,
        f.sql_conninfo,
        &format!("SELECT pg_create_restore_point('{label}')"),
    )?;
    let flush = psql(f.sql_psql, f.sql_conninfo, "SELECT pg_switch_wal()")?;
    if lsn(&marker)? <= stop {
        return Err(invalid("restore point does not follow the backup stop LSN"));
    }
    let last = lsn(&marker)? / f.segment_bytes;
    let deadline = std::time::Instant::now() + f.wal_wait;
    loop {
        let segments = wal_segments(f.root, f.segment_bytes)?;
        match contiguous(&segments, timeline, first, last, false) {
            Ok(()) => break,
            Err(e) if std::time::Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(Duration::from_secs(1)),
        }
    }
    let manifest = fs::read(tree.join("backup_manifest"))?;
    let checksum: String = Sha256::digest(&manifest)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let closes_gap = gap_closed_by(f.root, f.started)?;
    let epoch = if closes_gap {
        rotate(f.root)?
    } else {
        generation(f.root)?
    };
    let meta = json!({
        "backup_id": f.id,
        "source_host": f.source_host,
        "pg_major": major.parse::<u64>().map_err(|_| invalid("invalid PG_VERSION"))?,
        "timeline": timeline,
        "epoch_id": epoch,
        "system_identifier": system_identifier,
        "checksum": checksum,
        "post_backup_lsn": marker,
        "backup_stop_lsn": format!("{:X}/{:X}", stop >> 32, stop & 0xffff_ffff),
        "archive_flush_lsn": flush,
        "restore_point": label,
    });
    durable::write_json(
        &f.root.join("base").join(format!("{}.meta.json", f.id)),
        &meta,
    )?;
    durable::atomic_write(
        &f.root.join("LAST_SUCCESS"),
        format!("{}\n", f.id).as_bytes(),
    )?;
    if closes_gap {
        remove_gap(f.root)?;
    }
    Ok(meta)
}

// ------------------------------------------------------------------ health

pub struct Health<'a> {
    pub root: &'a Path,
    pub segment_bytes: u64,
    pub slot: &'a str,
    pub psql: &'a Path,
    pub conninfo: &'a str,
    pub systemctl: Option<(&'a Path, &'a str)>,
    pub max_wal_age: u64,
    pub max_base_age: Option<u64>,
}
fn age(path: &Path) -> Result<u64> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .map_or(0, |d| d.as_secs()))
}
/// Every failed condition, empty when healthy.
pub fn health(h: &Health) -> Vec<String> {
    let mut failures = Vec::new();
    let mut fail = |message: String| failures.push(message);
    match slot_state(h.psql, h.conninfo, h.slot) {
        Ok(Slot::Present) => {}
        Ok(Slot::Missing) => fail(format!("replication slot {} is missing", h.slot)),
        Ok(Slot::Lost) => fail(format!("replication slot {} is lost", h.slot)),
        Err(e) => fail(format!("slot probe failed: {e}")),
    }
    if let Some((systemctl, unit)) = h.systemctl {
        let active = Command::new(systemctl)
            .args(["is-active", "--quiet", unit])
            .status();
        if !active.is_ok_and(|s| s.success()) {
            fail(format!("receiver {unit} is not active"));
        }
    }
    if gap_path(h.root).exists() {
        fail("WAL gap marker present; take a new base backup".into());
    }
    match wal_segments(h.root, h.segment_bytes) {
        Err(e) => fail(format!("cannot list WAL: {e}")),
        Ok(segments) => {
            let newest = segments
                .iter()
                .map(|s| (s.0, s.1))
                .max_by_key(|k| (k.0, k.1));
            match segments
                .iter()
                .filter(|s| Some((s.0, s.1)) == newest)
                .filter_map(|s| age(&s.3).ok())
                .min()
            {
                None => fail("no received WAL".into()),
                Some(a) if a > h.max_wal_age => {
                    fail(format!("newest WAL is {a}s old (max {}s)", h.max_wal_age))
                }
                Some(_) => {}
            }
            match oldest_base_range(h.root, h.segment_bytes) {
                Err(e) => fail(format!("retained base backups unusable: {e}")),
                Ok((timeline, first)) => {
                    // The base's timeline and every later (promoted) one must be gapless.
                    let timelines: std::collections::BTreeSet<_> = segments
                        .iter()
                        .map(|s| s.0)
                        .filter(|t| *t >= timeline)
                        .collect();
                    if timelines.is_empty() {
                        fail(format!("no WAL on timeline {timeline}"));
                    }
                    for t in timelines {
                        let mine = segments.iter().filter(|s| s.0 == t).map(|s| s.1);
                        let (lo, hi) = (mine.clone().min().unwrap_or(0), mine.max().unwrap_or(0));
                        let start = if t == timeline { first } else { lo };
                        if let Err(e) = contiguous(&segments, t, start, hi, true) {
                            fail(format!("WAL not contiguous from the oldest base: {e}"));
                        }
                    }
                }
            }
        }
    }
    if let Some(max) = h.max_base_age {
        match age(&h.root.join("LAST_SUCCESS")) {
            Ok(a) if a <= max => {}
            Ok(a) => fail(format!("LAST_SUCCESS is {a}s old (max {max}s)")),
            Err(e) => fail(format!("LAST_SUCCESS unreadable: {e}")),
        }
    }
    failures
}
/// `(timeline, first segment)` of the oldest retained published base.
fn oldest_base_range(root: &Path, size: u64) -> Result<(u32, u64)> {
    let mut trees: Vec<_> = fs::read_dir(root.join("base"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().ends_with(".partial"))
        .map(|e| e.path())
        .collect();
    trees.sort();
    // Bases from before the last cleared gap cannot reach the newest WAL.
    // ponytail: bases without a meta sidecar are always considered; use recoveryMeta with health.
    let current = fs::read_to_string(root.join("GENERATION"))
        .ok()
        .map(|g| g.trim().to_owned());
    let metas: Vec<_> = trees
        .iter()
        .map(|tree| {
            let mut meta = tree.as_os_str().to_owned();
            meta.push(".meta.json");
            durable::read_json(Path::new(&meta)).ok()
        })
        .collect();
    // Once any base carries metadata, sidecar-less (pre-recoveryMeta) trees are not recovery bases.
    let has_meta = metas.iter().any(Option::is_some);
    let mut keep = metas.iter().map(|m| match (m, &current) {
        (Some(meta), Some(current)) => meta["epoch_id"] == current.as_str(),
        (None, _) => !has_meta,
        _ => true,
    });
    trees.retain(|_| keep.next().unwrap_or(true));
    let oldest = trees
        .first()
        .ok_or_else(|| invalid("no retained base backup"))?;
    let (timeline, first, _) = manifest_range(oldest, size)?;
    Ok((timeline, first))
}

// --------------------------------------------------------------- admission

pub enum Admission {
    Admitted(String),
    Refused(String),
}
/// `meminfo` is /proc/meminfo text. Swap is only required when swap exists.
pub fn admit(meminfo: &str, min_ram: u64, min_swap: u64) -> Result<Admission> {
    let kib = |key: &str| -> Result<u64> {
        let line = meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .ok_or_else(|| invalid(format!("meminfo lacks {key}")))?;
        let value: u64 = line
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .parse()
            .map_err(|_| invalid("invalid meminfo"))?;
        Ok(value * 1024)
    };
    let (ram, swap_total, swap_free) =
        (kib("MemAvailable:")?, kib("SwapTotal:")?, kib("SwapFree:")?);
    if min_ram > 0 && ram < min_ram {
        return Ok(Admission::Refused(format!(
            "insufficient RAM: available={ram} required={min_ram}"
        )));
    }
    if min_swap > 0 && swap_total > 0 && swap_free < min_swap {
        return Ok(Admission::Refused(format!(
            "insufficient swap: free={swap_free} required={min_swap}"
        )));
    }
    Ok(Admission::Admitted(format!(
        "memory admitted available_ram={ram} required_ram={min_ram} free_swap={swap_free} required_swap={min_swap}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEM: &str =
        "MemAvailable:    2097152 kB\nSwapTotal:   4194304 kB\nSwapFree:   1048576 kB\n";

    #[test]
    fn admission_checks_ram_and_swap_only_when_present() {
        assert!(matches!(
            admit(MEM, 1 << 30, 1 << 29).unwrap(),
            Admission::Admitted(_)
        ));
        assert!(matches!(
            admit(MEM, 3 << 30, 0).unwrap(),
            Admission::Refused(_)
        ));
        assert!(matches!(
            admit(MEM, 0, 2 << 30).unwrap(),
            Admission::Refused(_)
        ));
        let no_swap = "MemAvailable: 1024 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        assert!(matches!(
            admit(no_swap, 0, 1 << 40).unwrap(),
            Admission::Admitted(_)
        ));
        assert!(admit("MemAvailable: 1 kB\n", 0, 0).is_err());
    }
    #[test]
    fn slot_output_classification() {
        assert_eq!(parse_slot("||"), Slot::Missing);
        assert_eq!(parse_slot("physical|0/3000000|1"), Slot::Present);
        assert_eq!(parse_slot("physical||"), Slot::Lost);
    }
    #[test]
    fn segment_names_round_trip_and_contiguity_detects_holes() {
        let size = 16 << 20;
        let name = segment_name(1, 0x1ff, size);
        assert_eq!(name, "0000000100000001000000FF");
        assert_eq!(parse_segment(&name, size), Some((1, 0x1ff, false)));
        assert_eq!(
            parse_segment(&format!("{name}.partial"), size),
            Some((1, 0x1ff, true))
        );
        let seg = |s, p| (1, s, p, PathBuf::new());
        let have = [seg(4, false), seg(5, false), seg(6, true)];
        assert!(contiguous(&have, 1, 4, 6, true).is_ok());
        assert!(contiguous(&have, 1, 4, 6, false).is_err());
        assert!(contiguous(&have, 1, 3, 6, true).is_err());
        let hole = [seg(4, false), seg(6, false)];
        assert!(contiguous(&hole, 1, 4, 6, true).is_err());
    }
    #[test]
    fn gap_marks_clears_and_rotates_generation() {
        let root = std::env::temp_dir().join(format!("pgb-gap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("wal")).unwrap();
        fs::create_dir_all(root.join("base")).unwrap();
        let before = generation(&root).unwrap();
        assert_eq!(generation(&root).unwrap(), before);
        durable::write_json(&gap_path(&root), &json!({"detected_epoch": 100})).unwrap();
        assert!(
            !clear_gap(&root, 100).unwrap(),
            "a base started before the gap must not clear it"
        );
        assert!(gap_path(&root).exists());
        assert!(clear_gap(&root, 101).unwrap());
        assert!(!gap_path(&root).exists());
        assert_ne!(generation(&root).unwrap(), before);
        fs::remove_dir_all(&root).unwrap();
    }
}
