//! Recovery-ready pg-backup helpers: WAL gap marker, recovery metadata,
//! backup health and memory admission. Every file below `root` is published
//! durably; `root` is the per-source backup directory of `services.harbor-db.pgBackup`.
use super::{Result, durable, invalid, recovery};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Milliseconds: gap detection and a base start in the same second must stay ordered.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
    let per_log = (1u64 << 32) / size;
    // A segment field past the per-log count is not a canonical WAL name.
    let segment = part(16..24).filter(|s| *s < per_log)?;
    Some((
        u32::try_from(part(0..8)?).ok()?,
        part(8..16)? * per_log + segment,
        partial,
    ))
}
/// Received WAL by `(timeline, segment)`: `(partial, path)`; a complete file wins over its `.partial`.
type Archive = BTreeMap<(u32, u64), (bool, PathBuf)>;
fn wal_segments(root: &Path, size: u64) -> Result<Archive> {
    let mut out = Archive::new();
    for entry in fs::read_dir(root.join("wal"))? {
        let entry = entry?;
        if let Some((t, s, p)) = parse_segment(&entry.file_name().to_string_lossy(), size) {
            // A complete name must be a regular file of exactly one segment.
            if !p
                && !entry
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.len() == size)
            {
                continue;
            }
            if p && out.contains_key(&(t, s)) {
                continue;
            }
            out.insert((t, s), (p, entry.path()));
        }
    }
    Ok(out)
}
/// System identifier in the long page header of a segment's first page (0 while unwritten).
/// Same check inspect-backup makes: a manifest changed after finalization (or a
/// malformed recorded checksum) makes the base unusable. No sidecar, no check.
fn verify_manifest(tree: &Path) -> Result<()> {
    let mut sidecar = tree.as_os_str().to_owned();
    sidecar.push(".meta.json");
    let Ok(meta) = durable::read_json(Path::new(&sidecar)) else {
        return Ok(());
    };
    match meta.get("checksum") {
        None => Ok(()),
        Some(recorded) if manifest_sha256(tree).ok().as_deref() == recorded.as_str() => Ok(()),
        Some(_) => Err(invalid("backup manifest changed after finalization")),
    }
}
/// Hex SHA-256 of a base's `backup_manifest`, as recorded in its sidecar.
fn manifest_sha256(tree: &Path) -> Result<String> {
    let manifest = fs::read(tree.join("backup_manifest"))?;
    Ok(Sha256::digest(&manifest)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
/// System identifier of the newest segment with a written header.
fn newest_sysid(archive: &Archive) -> Option<u64> {
    archive
        .values()
        .rev()
        .map(|(_, path)| segment_sysid(path).unwrap_or(0))
        .find(|id| *id != 0)
}
fn segment_sysid(path: &Path) -> Result<u64> {
    let mut header = [0u8; 32];
    fs::File::open(path)?.read_exact(&mut header)?;
    Ok(u64::from_ne_bytes(header[24..32].try_into().unwrap()))
}
/// System identifier of a base: the first field of `global/pg_control`.
fn tree_sysid(tree: &Path) -> Result<u64> {
    let mut id = [0u8; 8];
    fs::File::open(tree.join("global/pg_control"))?.read_exact(&mut id)?;
    Ok(u64::from_ne_bytes(id))
}
/// Every archived segment `first..=last` belongs to the cluster `sysid`: stale
/// files of a replaced cluster must never satisfy continuity.
// ponytail: reads one 32-byte header per retained segment each run (linear in retention;
// ~1 s on Atlas today); cache verified segment headers if retention grows by orders.
fn same_cluster(archive: &Archive, timeline: u32, first: u64, last: u64, sysid: u64) -> Result<()> {
    for segment in first..=last {
        if let Some((partial, path)) = archive.get(&(timeline, segment)) {
            // A .partial shorter than a header is unwritten: its cluster is unknown.
            let found = match segment_sysid(path) {
                Err(_) if *partial => 0,
                other => other?,
            };
            if found != sysid && !(*partial && found == 0) {
                return Err(invalid(format!(
                    "WAL segment {segment} (timeline {timeline}) belongs to another cluster; use a fresh backup root"
                )));
            }
        }
    }
    Ok(())
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
    // ponytail: a standby backup crossing a promotion has one range per timeline; verify each leg if needed.
    if ranges.iter().any(|r| r["Timeline"] != first["Timeline"]) {
        return Err(invalid("multi-timeline backup manifests are unsupported"));
    }
    Ok((
        timeline(last)?,
        field(first, "Start-LSN")? / size,
        field(last, "End-LSN")?,
    ))
}
/// Every segment `first..=last` of `timeline` is present; only the newest may be partial.
fn contiguous(
    archive: &Archive,
    timeline: u32,
    first: u64,
    last: u64,
    allow_partial_last: bool,
) -> Result<()> {
    for segment in first..=last {
        match archive.get(&(timeline, segment)) {
            Some((partial, _)) if !*partial || (allow_partial_last && segment == last) => {}
            _ => {
                return Err(invalid(format!(
                    "WAL segment {segment} (timeline {timeline}) missing"
                )));
            }
        }
    }
    Ok(())
}
/// Switchpoints `(parent timeline, LSN)` from a `.history` file, oldest first.
fn parse_history(text: &str) -> Result<Vec<(u32, u64)>> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut f = l.split_whitespace();
            let tli = f.next().and_then(|t| t.parse().ok());
            let at = f.next().map(lsn);
            match (tli, at) {
                (Some(tli), Some(Ok(at))) => Ok((tli, at)),
                _ => Err(invalid("unreadable history file")),
            }
        })
        .collect()
}
/// Gapless legs `(timeline, first, last)` from the base's start to the newest
/// segment of `newest`, following the newest timeline's history.
fn chain(
    root: &Path,
    size: u64,
    base: u32,
    first: u64,
    stop: u64,
    newest: u32,
    hi: u64,
) -> Result<Vec<(u32, u64, u64)>> {
    if newest == base {
        // The archive must reach the base's stop, not merely exist on its timeline.
        if hi < stop / size {
            return Err(invalid("archived WAL ends before the base's stop LSN"));
        }
        return Ok(vec![(base, first, hi)]);
    }
    let text = fs::read_to_string(root.join("wal").join(format!("{newest:08X}.history")))
        .map_err(|_| invalid(format!("history file of timeline {newest} missing")))?;
    let history = parse_history(&text)?;
    let at = history
        .iter()
        .position(|(t, _)| *t == base)
        .ok_or_else(|| {
            invalid(format!(
                "base timeline {base} is not an ancestor of {newest}"
            ))
        })?;
    // The base holds state up to its stop LSN; a branch that left earlier cannot recover it.
    if history[at].1 < stop {
        return Err(invalid(format!(
            "timeline {newest} forked before the base's stop LSN"
        )));
    }
    let (mut legs, mut start) = (Vec::new(), first);
    for (timeline, fork) in &history[at..] {
        let fork_at = *fork;
        let fork = fork_at / size;
        // At an exact boundary the fork segment belongs to the child alone.
        let end = if fork_at % size == 0 {
            fork.saturating_sub(1)
        } else {
            fork
        };
        legs.push((*timeline, start, end));
        start = fork;
    }
    if hi < start {
        return Err(invalid(format!("timeline {newest} ends before its fork")));
    }
    legs.push((newest, start, hi));
    Ok(legs)
}

// ----------------------------------------------------- slot state and psql

#[derive(Debug, PartialEq)]
pub enum Slot {
    Missing,
    Lost,
    /// Restart LSN of the slot.
    Present(u64),
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
        lsn(restart).map_or(Slot::Lost, Slot::Present)
    }
}
/// psql exits 2 when it cannot connect at all.
const UNREACHABLE: &str = "psql could not connect: ";
fn unreachable(e: &super::StorageError) -> bool {
    e.to_string().contains(UNREACHABLE)
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
        let prefix = if output.status.code() == Some(2) {
            UNREACHABLE
        } else {
            "psql failed: "
        };
        return Err(invalid(format!(
            "{prefix}{}",
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
/// Receiver start-up: a replaced cluster or a slot that must be recreated over
/// existing history is a WAL gap.
pub fn mark_gap(
    root: &Path,
    segment_bytes: u64,
    psql_bin: &Path,
    conninfo: &str,
    slot: &str,
) -> Result<bool> {
    // An unreachable source defers the check: the receiver cannot stream either, and
    // Restart= reruns this before it next connects (health still checks contiguity).
    let state = match slot_state(psql_bin, conninfo, slot) {
        Err(e) if unreachable(&e) => {
            eprintln!("source unreachable; WAL gap check deferred to the next receiver start");
            return Ok(false);
        }
        state => state?,
    };
    // An invalidated slot cannot be resumed and --if-not-exists would keep it: replace it.
    if state == Slot::Lost {
        psql(psql_bin, conninfo, &format!("DROP_REPLICATION_SLOT {slot}"))?;
    }
    // A loss detected while a gap is already recorded refreshes its timestamp,
    // so only a base started after the latest loss closes it.
    let archive = wal_segments(root, segment_bytes)?;
    let identify = psql(psql_bin, conninfo, "IDENTIFY_SYSTEM")?;
    let mut fields = identify.split('|');
    let server: u64 = fields
        .next()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| invalid("unreadable IDENTIFY_SYSTEM"))?;
    let server_timeline: u32 = fields.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    // A server behind the archive's newest timeline was restored and re-promoted: a sibling branch.
    if archive.keys().any(|k| k.0 > server_timeline) && server_timeline != 0 {
        return write_gap(
            root,
            slot,
            "timeline went backwards; use a fresh backup root",
        );
    }
    // The newest segment with a written header; an unwritten .partial reads 0.
    if newest_sysid(&archive).is_some_and(|found| found != server) {
        return write_gap(
            root,
            slot,
            "system identifier changed; use a fresh backup root",
        );
    }
    let oldest = oldest_base_range(root, segment_bytes).ok();
    // ponytail: coverage is proven only within one timeline. A server on another timeline
    // than the oldest retained base records a gap, so the next base anchors on the new
    // timeline; prove multi-timeline legs here if promotions ever become routine.
    if timeline_changed(oldest.as_ref().map(|o| o.0), server_timeline) {
        return write_gap(
            root,
            slot,
            "timeline changed since the oldest retained base; take a new base backup",
        );
    }
    if let Slot::Present(restart) = state {
        // A slot recreated while the receiver was down restarts past what the archive holds.
        let on_server = archive
            .range((server_timeline, 0)..=(server_timeline, u64::MAX))
            .next_back()
            .map(|(k, _)| k.1);
        let has_base = fs::read_dir(root.join("base"))?.next().is_some();
        let covered = match on_server {
            Some(n) => restart / segment_bytes <= n + 1,
            // An empty archive next to retained bases means the WAL itself is gone.
            None => archive.is_empty() && !has_base,
        };
        // A hole inside any retained timeline is a gap too.
        let hole = has_hole(&archive);
        // So is WAL missing before the first archived segment the oldest base needs.
        let lead = oldest.is_some_and(|(t, first, _, _)| leading_gap(&archive, t, first));
        if covered && !hole && !lead {
            return Ok(false);
        }
        return write_gap(root, slot, "slot restart point is beyond the archive");
    }
    if archive.is_empty() && fs::read_dir(root.join("base"))?.next().is_none() {
        return Ok(false);
    }
    write_gap(root, slot, "slot recreated")
}
/// `post_backup_lsn` of the base LAST_SUCCESS names, when it has metadata.
/// The newest base's recovery target and the timeline it lies on.
/// Roots without any sidecar keep legacy timestamp markers and have no target; once
/// recovery metadata is in use, an unusable `LAST_SUCCESS` sidecar is an error.
fn recovery_target(root: &Path, configured: bool) -> Result<Option<(u64, u64)>> {
    let uses_sidecars = fs::read_dir(root.join("base"))?
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".meta.json"));
    // A legacy marker is a timestamp; one naming a base directory is an ID marker,
    // so metadata mode survives even if every sidecar has been lost.
    let id_marker = fs::read_to_string(root.join("LAST_SUCCESS"))
        .is_ok_and(|m| !m.trim().is_empty() && root.join("base").join(m.trim()).is_dir());
    let uses_meta = configured || uses_sidecars || id_marker;
    if !uses_meta {
        return Ok(None);
    }
    // inspect-backup selects through the marker, so missing or unreadable is unusable.
    let id = fs::read_to_string(root.join("LAST_SUCCESS"))
        .map_err(|_| invalid("LAST_SUCCESS is missing or unreadable"))?;
    let tree = root.join("base").join(id.trim());
    let path = root.join("base").join(format!("{}.meta.json", id.trim()));
    let meta = durable::read_json(&path).ok();
    verify_manifest(&tree)?;
    let target = meta.as_ref().and_then(|m| {
        Some((
            m["timeline"].as_u64()?,
            lsn(m["post_backup_lsn"].as_str()?).ok()?,
        ))
    });
    target
        .map(Some)
        .ok_or_else(|| invalid("LAST_SUCCESS recovery metadata is missing or unusable"))
}
/// LSN the archive must reach: the target counts when it lies on the newest timeline,
/// otherwise it must lie on an ancestor before that ancestor's fork, so the descendant
/// chain covers it. A target on an unarchived or sibling timeline is unreachable.
fn reach(root: &Path, target: Option<(u64, u64)>, newest: u32, stop: u64) -> Result<u64> {
    let Some((t, lsn)) = target else {
        return Ok(stop);
    };
    if t > u64::from(newest) {
        return Err(invalid(format!(
            "recovery target is on timeline {t}, newest archived WAL is on {newest}"
        )));
    }
    if t == u64::from(newest) {
        return Ok(lsn.max(stop));
    }
    match fork_of(root, newest, t) {
        Some(fork) if lsn <= fork => Ok(stop),
        _ => Err(invalid(format!(
            "recovery target on timeline {t} is not on the ancestry of timeline {newest}"
        ))),
    }
}
/// LSN where `ancestor` forked on the way to `timeline`, from `timeline`'s history file.
fn fork_of(root: &Path, timeline: u32, ancestor: u64) -> Option<u64> {
    let text = fs::read_to_string(root.join("wal").join(format!("{timeline:08X}.history"))).ok()?;
    parse_history(&text)
        .ok()?
        .into_iter()
        .find(|(t, _)| u64::from(*t) == ancestor)
        .map(|(_, at)| at)
}
/// Last segment that must be complete: the one holding the recovery target (or the
/// base stop); only a later tail may still be a `.partial`.
fn complete_through(target: Option<u64>, stop: u64, segment_bytes: u64) -> u64 {
    target.unwrap_or(stop).max(stop) / segment_bytes
}
/// An interrupted `remove_gap` leaves its marker as `.gap.clearing.*`; it still counts.
fn gap_present(root: &Path) -> bool {
    gap_path(root).exists() || clearing_markers(root).next().is_some()
}
fn clearing_markers(root: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(root.join("wal"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".gap.clearing."))
        })
}
/// Put a marker orphaned by a crash back as `.gap` unless a newer one is there.
/// ponytail: assumes a single finalizer at a time; add a lock if that changes.
fn recover_clearing(root: &Path) -> Result<()> {
    for orphan in clearing_markers(root).collect::<Vec<_>>() {
        let ms = |p: &Path| {
            durable::read_json(p)
                .ok()
                .and_then(|g| g["detected_ms"].as_u64())
        };
        if ms(&orphan) > ms(&gap_path(root)) {
            fs::rename(&orphan, gap_path(root))?;
        } else {
            fs::remove_file(&orphan)?;
        }
    }
    durable::sync_directory(&root.join("wal"))
}
/// The server left the oldest retained base's timeline (0 means unknown).
fn timeline_changed(base: Option<u32>, server: u32) -> bool {
    base.is_some_and(|t| server != 0 && t != server)
}
/// The archive's first segment on `timeline` starts after `first`, or it has none.
fn leading_gap(archive: &Archive, timeline: u32, first: u64) -> bool {
    archive
        .range((timeline, 0)..=(timeline, u64::MAX))
        .next()
        .is_none_or(|(k, _)| k.1 > first)
}
/// A missing segment between the first and last retained segment of any timeline.
fn has_hole(archive: &Archive) -> bool {
    let timelines: std::collections::BTreeSet<u32> = archive.keys().map(|k| k.0).collect();
    timelines.into_iter().any(|t| {
        let mut segs = archive.range((t, 0)..=(t, u64::MAX)).map(|(k, _)| k.1);
        match (segs.next(), segs.next_back()) {
            (Some(lo), Some(hi)) => contiguous(archive, t, lo, hi, true).is_err(),
            _ => false,
        }
    })
}
fn write_gap(root: &Path, slot: &str, reason: &str) -> Result<bool> {
    let generation = generation(root)?;
    durable::write_json(
        &gap_path(root),
        &json!({"slot": slot, "reason": reason, "detected_ms": now(), "generation": generation}),
    )?;
    Ok(true)
}
/// Base started at `started` and verified: it post-dates the gap, so it closes it.
/// Returns the closed marker's `detected_ms`, its identity for `remove_gap`.
fn gap_closed_by(root: &Path, started: u64) -> Result<Option<u64>> {
    recover_clearing(root)?;
    match durable::read_json(&gap_path(root)) {
        Ok(gap) => Ok(gap["detected_ms"].as_u64().filter(|t| *t < started)),
        Err(_) if !gap_path(root).exists() => Ok(None),
        Err(e) => Err(e),
    }
}
/// Bases that started before the cleared gap cannot reach the newest WAL.
fn set_floor(root: &Path, started: u64) -> Result<()> {
    durable::atomic_write(&root.join("BASE_FLOOR"), format!("{started}\n").as_bytes())
}
/// Compare-and-remove: only the marker seen at check time (`detected_ms`) is
/// cleared; a newer one the receiver wrote since survives.
fn remove_gap(root: &Path, seen: u64) -> Result<()> {
    let taken = root.join(format!("wal/.gap.clearing.{}", std::process::id()));
    match fs::rename(gap_path(root), &taken) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    if durable::read_json(&taken)?["detected_ms"].as_u64() != Some(seen) {
        // Newer marker: put it back unless an even newer one already exists.
        match fs::hard_link(&taken, gap_path(root)) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
            _ => {}
        }
    }
    fs::remove_file(&taken)?;
    durable::sync_directory(&root.join("wal"))
}
/// Legacy-marker deployments: clear a gap after a verified later base.
pub fn clear_gap(root: &Path, started: u64) -> Result<bool> {
    let Some(seen) = gap_closed_by(root, started)? else {
        return Ok(false);
    };
    rotate(root)?;
    set_floor(root, started)?;
    remove_gap(root, seen)?;
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
    // ponytail: standby sources are unsupported for recoveryMeta (restore points need a primary).
    if psql(f.sql_psql, f.sql_conninfo, "SELECT pg_is_in_recovery()")? == "t" {
        return Err(invalid(
            "recoveryMeta needs sqlConninfo to reach a primary, not a standby",
        ));
    }
    let live = psql(
        f.sql_psql,
        f.sql_conninfo,
        "SELECT system_identifier FROM pg_control_system()",
    )?;
    if live != system_identifier {
        return Err(invalid(
            "sqlConninfo reaches a different cluster than the base backup",
        ));
    }
    let marker = psql(
        f.sql_psql,
        f.sql_conninfo,
        &format!("SELECT pg_create_restore_point('{label}')"),
    )?;
    let flush = psql(f.sql_psql, f.sql_conninfo, "SELECT pg_switch_wal()")?;
    if lsn(&marker)? <= stop {
        return Err(invalid("restore point does not follow the backup stop LSN"));
    }
    // A promotion between the manifest and the restore point would put the marker on another timeline.
    let marker_timeline = psql(
        f.sql_psql,
        f.sql_conninfo,
        &format!("SELECT substr(pg_walfile_name('{marker}'), 1, 8)"),
    )?;
    if u32::from_str_radix(&marker_timeline, 16).ok() != Some(timeline) {
        return Err(invalid(format!(
            "restore point is on timeline {marker_timeline}, the base is on {timeline}"
        )));
    }
    let last = lsn(&marker)? / f.segment_bytes;
    let deadline = std::time::Instant::now() + f.wal_wait;
    let archive = loop {
        let archive = wal_segments(f.root, f.segment_bytes)?;
        match contiguous(&archive, timeline, first, last, false) {
            Ok(()) => break archive,
            Err(e) if std::time::Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(Duration::from_secs(1)),
        }
    };
    let sysid: u64 = system_identifier
        .parse()
        .map_err(|_| invalid("invalid system identifier"))?;
    same_cluster(&archive, timeline, first, last, sysid)?;
    let checksum = manifest_sha256(&tree)?;
    let closes_gap = gap_closed_by(f.root, f.started)?;
    let epoch = if closes_gap.is_some() {
        set_floor(f.root, f.started)?;
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
    if let Some(seen) = closes_gap {
        remove_gap(f.root, seen)?;
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
    /// Configured recoveryMeta: metadata mode regardless of what is left on disk.
    pub recovery_meta: bool,
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
        Ok(Slot::Present(_)) => {}
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
    if gap_present(h.root) {
        fail("WAL gap marker present; take a new base backup".into());
    }
    match wal_segments(h.root, h.segment_bytes) {
        Err(e) => fail(format!("cannot list WAL: {e}")),
        Ok(archive) => {
            match archive.iter().next_back() {
                None => fail("no received WAL".into()),
                Some((_, (_, path))) => match age(path) {
                    Ok(a) if a > h.max_wal_age => {
                        fail(format!("newest WAL is {a}s old (max {}s)", h.max_wal_age))
                    }
                    Ok(_) => {}
                    Err(e) => fail(format!("newest WAL unreadable: {e}")),
                },
            }
            match oldest_base_range(h.root, h.segment_bytes) {
                Err(e) => fail(format!("retained base backups unusable: {e}")),
                Ok((timeline, first, stop, tree)) => {
                    // The newest published base's recovery target must be archived too.
                    let newest = archive.keys().map(|k| k.0).max().unwrap_or(0);
                    let hi = archive
                        .range((newest, 0)..=(newest, u64::MAX))
                        .next_back()
                        .map_or(0, |(k, _)| k.1);
                    let needed = recovery_target(h.root, h.recovery_meta)
                        .and_then(|target| reach(h.root, target, newest, stop));
                    let complete_through =
                        complete_through(needed.as_ref().ok().copied(), stop, h.segment_bytes);
                    let legs = match needed {
                        _ if newest < timeline => {
                            Err(invalid(format!("no WAL on timeline {timeline}")))
                        }
                        Err(e) => Err(e),
                        Ok(reach) if hi < reach / h.segment_bytes => {
                            Err(invalid("archived WAL ends before the recovery target"))
                        }
                        Ok(_) => chain(h.root, h.segment_bytes, timeline, first, stop, newest, hi),
                    };
                    match (legs, tree_sysid(&tree)) {
                        (Ok(legs), Ok(sysid)) => {
                            let last = legs.len().saturating_sub(1);
                            for (i, (t, lo, end)) in legs.into_iter().enumerate() {
                                // A .partial file has unknown fill: only the final leg's tail,
                                // past the stop and the recovery target, may be one.
                                let partial_ok = i == last && end > complete_through;
                                if let Err(e) = contiguous(&archive, t, lo, end, partial_ok)
                                    .and_then(|()| same_cluster(&archive, t, lo, end, sysid))
                                {
                                    fail(format!("WAL not usable from the oldest base: {e}"));
                                }
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => fail(format!("WAL chain: {e}")),
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
/// `(timeline, first segment, stop LSN, tree)` of the oldest retained published base.
fn oldest_base_range(root: &Path, size: u64) -> Result<(u32, u64, u64, PathBuf)> {
    let mut trees: Vec<_> = fs::read_dir(root.join("base"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().ends_with(".partial"))
        .map(|e| e.path())
        .collect();
    // Oldest first by publish time (names tie within a second); name breaks ties.
    trees.sort_by_key(|t| (fs::metadata(t).and_then(|m| m.modified()).ok(), t.clone()));
    // Bases from before the last cleared gap cannot reach the newest WAL.
    let floor: u64 = fs::read_to_string(root.join("BASE_FLOOR"))
        .ok()
        .and_then(|f| f.trim().parse().ok())
        .unwrap_or(0);
    trees.retain(|tree| {
        fs::metadata(tree)
            .and_then(|m| m.modified())
            .is_ok_and(|t| {
                t.duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64)
                    >= floor
            })
    });
    // Once any base carries recovery metadata, failed-finalize (sidecar-less) trees are not recovery bases.
    let has_meta = |t: &PathBuf| {
        let mut m = t.as_os_str().to_owned();
        m.push(".meta.json");
        Path::new(&m).is_file()
    };
    if trees.iter().any(has_meta) {
        trees.retain(has_meta);
    }
    let oldest = trees
        .first()
        .ok_or_else(|| invalid("no retained base backup"))?;
    // The anchor's range is trusted only while its manifest matches its sidecar.
    verify_manifest(oldest)?;
    let (timeline, first, stop) = manifest_range(oldest, size)?;
    Ok((timeline, first, stop, oldest.clone()))
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
    fn present_slot_parses_restart_lsn() {
        assert_eq!(parse_slot("physical|0/5000000|1"), Slot::Present(0x5000000));
        assert_eq!(parse_slot("physical|bogus|1"), Slot::Lost);
    }
    #[test]
    fn slot_output_classification() {
        assert_eq!(parse_slot("||"), Slot::Missing);
        assert_eq!(parse_slot("physical|0/3000000|1"), Slot::Present(0x3000000));
        assert_eq!(parse_slot("physical||"), Slot::Lost);
    }
    fn archive(parts: &[(u32, u64, bool)]) -> Archive {
        parts
            .iter()
            .map(|(t, s, p)| ((*t, *s), (*p, PathBuf::new())))
            .collect()
    }
    #[test]
    fn segment_names_round_trip_and_contiguity_detects_holes() {
        let size = 16 << 20;
        let name = segment_name(1, 0x1ff, size);
        assert_eq!(name, "0000000100000001000000FF");
        // 16 MiB segments give 256 per log: a segment field of 0x100 is not canonical.
        assert!(parse_segment("000000010000000000000100", 16 << 20).is_none());
        assert_eq!(parse_segment(&name, size), Some((1, 0x1ff, false)));
        assert_eq!(
            parse_segment(&format!("{name}.partial"), size),
            Some((1, 0x1ff, true))
        );
        let have = archive(&[(1, 4, false), (1, 5, false), (1, 6, true)]);
        assert!(contiguous(&have, 1, 4, 6, true).is_ok());
        assert!(contiguous(&have, 1, 4, 6, false).is_err());
        assert!(contiguous(&have, 1, 3, 6, true).is_err());
        assert!(contiguous(&archive(&[(1, 4, false), (1, 6, false)]), 1, 4, 6, true).is_err());
    }
    #[test]
    fn chain_requires_each_parent_through_its_fork() {
        let root = std::env::temp_dir().join(format!("pgb-chain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("wal")).unwrap();
        fs::write(
            root.join("wal/00000002.history"),
            "1\t0/5000060\tno recovery target specified\n",
        )
        .unwrap();
        let size = 16 << 20;
        assert_eq!(
            chain(&root, size, 1, 3, 0x3000000, 1, 9).unwrap(),
            vec![(1, 3, 9)]
        );
        let legs = chain(&root, size, 1, 3, 0x4000000, 2, 8).unwrap();
        assert!(
            chain(&root, size, 1, 3, 0x6000000, 2, 8).is_err(),
            "base stop beyond the fork"
        );
        assert_eq!(legs, vec![(1, 3, 5), (2, 5, 8)]);
        fs::write(root.join("wal/00000005.history"), "1\t0/5000000\tx\n").unwrap();
        assert_eq!(
            chain(&root, size, 1, 3, 0x4000000, 5, 8).unwrap(),
            vec![(1, 3, 4), (5, 5, 8)],
            "boundary fork"
        );
        // Parent segment 4 missing while the child is intact: the second leg alone passes.
        let have = archive(&[
            (1, 3, false),
            (1, 5, true),
            (2, 5, false),
            (2, 6, false),
            (2, 7, false),
            (2, 8, true),
        ]);
        let failed = legs
            .iter()
            .filter(|(t, a, b)| contiguous(&have, *t, *a, *b, true).is_err())
            .count();
        assert_eq!(failed, 1);
        assert!(
            chain(&root, size, 1, 3, 0x3000000, 3, 9).is_err(),
            "missing history of the newest timeline"
        );
        assert!(
            chain(&root, size, 7, 3, 0x3000000, 2, 9).is_err(),
            "base timeline not an ancestor"
        );
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn short_complete_wal_files_are_not_indexed() {
        let root = std::env::temp_dir().join(format!("pgb-short-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("wal")).unwrap();
        let size = 1 << 20;
        let full = root.join(segment_name(1, 4, size));
        fs::File::create(&full).unwrap().set_len(size).unwrap();
        fs::write(root.join(segment_name(1, 5, size)), vec![0u8; 64]).unwrap();
        fs::write(
            root.join("wal")
                .join(format!("{}.partial", segment_name(1, 6, size))),
            b"x",
        )
        .unwrap();
        fs::rename(&full, root.join("wal").join(segment_name(1, 4, size))).unwrap();
        fs::rename(
            root.join(segment_name(1, 5, size)),
            root.join("wal").join(segment_name(1, 5, size)),
        )
        .unwrap();
        let archive = wal_segments(&root, size).unwrap();
        assert!(archive.contains_key(&(1, 4)) && archive.contains_key(&(1, 6)));
        assert!(
            !archive.contains_key(&(1, 5)),
            "truncated file must not count"
        );
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn health_ignores_failed_finalize_trees_older_than_the_valid_base() {
        let root = std::env::temp_dir().join(format!("pgb-oldest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let size = 16u64 << 20;
        for (id, start) in [("a-failed", "0/1000000"), ("b-valid", "0/5000000")] {
            let t = root.join("base").join(id);
            fs::create_dir_all(&t).unwrap();
            let m = json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":start,"End-LSN":"0/6000000"}]});
            durable::write_json(&t.join("backup_manifest"), &m).unwrap();
        }
        fs::write(root.join("base/b-valid.meta.json"), "{}").unwrap();
        let (_, first, _, tree) = oldest_base_range(&root, size).unwrap();
        assert_eq!((first, tree.ends_with("b-valid")), (5, true));
        // The anchor's manifest must still match its recorded checksum.
        let sum = manifest_sha256(&root.join("base/b-valid")).unwrap();
        durable::write_json(
            &root.join("base/b-valid.meta.json"),
            &json!({"checksum": sum}),
        )
        .unwrap();
        assert!(oldest_base_range(&root, size).is_ok());
        let edited =
            json!({"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/5800000","End-LSN":"0/6000000"}]});
        durable::write_json(&root.join("base/b-valid/backup_manifest"), &edited).unwrap();
        assert!(oldest_base_range(&root, size).is_err());
        fs::remove_file(root.join("base/b-valid.meta.json")).unwrap();
        assert_eq!(
            oldest_base_range(&root, size).unwrap().1,
            1,
            "no metadata anywhere: all trees count"
        );
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn partial_only_segments_do_not_satisfy_a_leg_end() {
        let have = archive(&[(1, 4, false), (1, 5, true)]);
        assert!(contiguous(&have, 1, 4, 5, false).is_err());
        assert!(contiguous(&have, 1, 4, 5, true).is_ok());
    }
    #[test]
    fn holes_are_found_on_any_timeline() {
        let ok = archive(&[(1, 3, false), (1, 4, false), (2, 5, false)]);
        assert!(!has_hole(&ok));
        let ancestor = archive(&[(1, 3, false), (1, 5, false), (2, 6, false), (2, 7, false)]);
        assert!(has_hole(&ancestor));
    }
    #[test]
    fn the_target_segment_must_be_complete() {
        let mib = 1 << 20;
        // Target in segment 9, stop in 5: a partial is allowed only from segment 10.
        assert_eq!(complete_through(Some(9 * mib + 40), 5 * mib, mib), 9);
        assert_eq!(complete_through(None, 5 * mib, mib), 5);
        assert_eq!(complete_through(Some(3 * mib), 5 * mib, mib), 5);
    }
    #[test]
    fn a_source_that_cannot_be_reached_is_classified() {
        assert!(unreachable(&invalid(format!("{UNREACHABLE}timeout"))));
        assert!(!unreachable(&invalid("psql failed: syntax error")));
    }
    #[test]
    fn a_promotion_past_the_oldest_base_is_a_gap() {
        assert!(timeline_changed(Some(1), 2));
        assert!(timeline_changed(Some(3), 2));
        assert!(!timeline_changed(Some(2), 2));
        assert!(!timeline_changed(Some(2), 0));
        assert!(!timeline_changed(None, 2));
    }
    #[test]
    fn wal_missing_before_the_archive_start_is_a_leading_gap() {
        let tail = archive(&[(1, 110, false), (1, 120, false)]);
        assert!(leading_gap(&tail, 1, 100));
        assert!(!leading_gap(&tail, 1, 110));
        assert!(leading_gap(&tail, 2, 100));
    }
    #[test]
    fn recovery_target_reads_last_success_metadata() {
        let root = std::env::temp_dir().join(format!("pgb-target-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("base")).unwrap();
        assert_eq!(recovery_target(&root, false).unwrap(), None);
        fs::write(root.join("LAST_SUCCESS"), "b1\n").unwrap();
        durable::write_json(
            &root.join("base/b1.meta.json"),
            &json!({"timeline":2,"post_backup_lsn":"0/7000028"}),
        )
        .unwrap();
        assert_eq!(recovery_target(&root, false).unwrap(), Some((2, 0x7000028)));
        // Once sidecars are in use, a LAST_SUCCESS without a usable one fails closed.
        fs::write(root.join("LAST_SUCCESS"), "b2\n").unwrap();
        assert!(recovery_target(&root, false).is_err());
        fs::write(root.join("base/b2.meta.json"), "{}").unwrap();
        assert!(recovery_target(&root, false).is_err());
        // A manifest changed after finalization is rejected, as inspect-backup does.
        fs::write(root.join("LAST_SUCCESS"), "b1\n").unwrap();
        fs::create_dir_all(root.join("base/b1")).unwrap();
        fs::write(root.join("base/b1/backup_manifest"), "original").unwrap();
        let sum = manifest_sha256(&root.join("base/b1")).unwrap();
        durable::write_json(
            &root.join("base/b1.meta.json"),
            &json!({"timeline":2,"post_backup_lsn":"0/7000028","checksum":sum}),
        )
        .unwrap();
        assert!(recovery_target(&root, false).is_ok());
        // A present checksum that is not a string is malformed, not absent.
        durable::write_json(
            &root.join("base/b1.meta.json"),
            &json!({"timeline":2,"post_backup_lsn":"0/7000028","checksum":null}),
        )
        .unwrap();
        assert!(recovery_target(&root, false).is_err());
        durable::write_json(
            &root.join("base/b1.meta.json"),
            &json!({"timeline":2,"post_backup_lsn":"0/7000028","checksum":sum}),
        )
        .unwrap();
        fs::write(root.join("base/b1/backup_manifest"), "tampered").unwrap();
        assert!(recovery_target(&root, false).is_err());
        fs::write(root.join("base/b1/backup_manifest"), "original").unwrap();
        fs::write(root.join("LAST_SUCCESS"), "b2\n").unwrap();
        // So does a marker that cannot be read, or none at all.
        fs::write(root.join("LAST_SUCCESS"), [0xff, 0xfe]).unwrap();
        assert!(recovery_target(&root, false).is_err());
        fs::remove_file(root.join("LAST_SUCCESS")).unwrap();
        assert!(recovery_target(&root, false).is_err());
        // An ID marker still means metadata mode after every sidecar is gone.
        for sidecar in ["b1", "b2"] {
            let _ = fs::remove_file(root.join(format!("base/{sidecar}.meta.json")));
        }
        fs::write(root.join("LAST_SUCCESS"), "b1\n").unwrap();
        assert!(recovery_target(&root, false).is_err());
        // A legacy timestamp marker without sidecars has no target...
        fs::write(root.join("LAST_SUCCESS"), "2026-11-01T12:00:00+00:00\n").unwrap();
        assert_eq!(recovery_target(&root, false).unwrap(), None);
        // ...unless recoveryMeta is configured: then nothing on disk downgrades it.
        assert!(recovery_target(&root, true).is_err());
        fs::remove_file(root.join("LAST_SUCCESS")).unwrap();
        assert!(recovery_target(&root, true).is_err());
        fs::write(root.join("LAST_SUCCESS"), "2026-11-01T12:00:00+00:00\n").unwrap();
        durable::write_json(
            &root.join("base/b1.meta.json"),
            &json!({"timeline":2,"post_backup_lsn":"0/7000028"}),
        )
        .unwrap();
        fs::write(root.join("LAST_SUCCESS"), "b1\n").unwrap();
        // An ancestor-timeline base must still reach a target on the newest timeline.
        assert_eq!(
            reach(&root, recovery_target(&root, false).unwrap(), 2, 0x5000000).unwrap(),
            0x7000028
        );
        // A target on a timeline with no archived WAL can never be reached.
        assert!(reach(&root, recovery_target(&root, false).unwrap(), 1, 0x5000000).is_err());
        // An older target timeline counts only on the newest timeline's ancestry.
        fs::create_dir_all(root.join("wal")).unwrap();
        assert!(reach(&root, recovery_target(&root, false).unwrap(), 3, 0x5000000).is_err());
        fs::write(
            root.join("wal/00000003.history"),
            "1\t0/3000000\tx\n2\t0/8000000\tx\n",
        )
        .unwrap();
        assert_eq!(
            reach(&root, recovery_target(&root, false).unwrap(), 3, 0x5000000).unwrap(),
            0x5000000
        );
        // A sibling branch, or a target after the ancestor's fork, is unreachable.
        fs::write(root.join("wal/00000003.history"), "1\t0/3000000\tx\n").unwrap();
        assert!(reach(&root, recovery_target(&root, false).unwrap(), 3, 0x5000000).is_err());
        fs::write(
            root.join("wal/00000003.history"),
            "1\t0/3000000\tx\n2\t0/6000000\tx\n",
        )
        .unwrap();
        assert!(reach(&root, recovery_target(&root, false).unwrap(), 3, 0x5000000).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn stale_wal_of_another_cluster_is_rejected() {
        let dir = std::env::temp_dir().join(format!("pgb-sysid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, id: u64| {
            let mut bytes = vec![0u8; 64];
            bytes[24..32].copy_from_slice(&id.to_ne_bytes());
            fs::write(dir.join(name), bytes).unwrap();
            dir.join(name)
        };
        let mut have = Archive::new();
        have.insert((1, 4), (false, write("a", 77)));
        have.insert((1, 5), (true, write("b", 0)));
        assert!(
            same_cluster(&have, 1, 4, 5, 77).is_ok(),
            "unwritten partial is tolerated"
        );
        fs::write(dir.join("short"), [0u8; 8]).unwrap();
        have.insert((1, 6), (true, dir.join("short")));
        assert!(
            same_cluster(&have, 1, 4, 6, 77).is_ok(),
            "a partial shorter than a header is unknown"
        );
        have.remove(&(1, 6));
        assert!(same_cluster(&have, 1, 4, 5, 78).is_err());
        // Replacement detection reads past an unwritten newest partial.
        assert_eq!(newest_sysid(&have), Some(77));
        have.insert((1, 5), (false, write("c", 78)));
        assert_eq!(newest_sysid(&have), Some(78));
        assert!(same_cluster(&have, 1, 4, 5, 77).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn multi_timeline_manifest_is_rejected() {
        let dir = std::env::temp_dir().join(format!("pgb-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let write = |ranges: Value| {
            durable::write_json(&dir.join("backup_manifest"), &json!({"WAL-Ranges": ranges}))
                .unwrap()
        };
        write(json!([{"Timeline":1,"Start-LSN":"0/3000000","End-LSN":"0/4000000"}]));
        assert_eq!(manifest_range(&dir, 16 << 20).unwrap(), (1, 3, 0x4000000));
        write(json!([
            {"Timeline":1,"Start-LSN":"0/3000000","End-LSN":"0/4000000"},
            {"Timeline":2,"Start-LSN":"0/4000000","End-LSN":"0/5000000"}
        ]));
        assert!(manifest_range(&dir, 16 << 20).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn finalize_style_floor_excludes_older_bases() {
        let root = std::env::temp_dir().join(format!("pgb-floor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        set_floor(&root, 1234).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("BASE_FLOOR")).unwrap().trim(),
            "1234"
        );
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn gap_marks_clears_and_rotates_generation() {
        let root = std::env::temp_dir().join(format!("pgb-gap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("wal")).unwrap();
        fs::create_dir_all(root.join("base")).unwrap();
        let before = generation(&root).unwrap();
        assert_eq!(generation(&root).unwrap(), before);
        durable::write_json(&gap_path(&root), &json!({"detected_ms": 100})).unwrap();
        assert!(
            !clear_gap(&root, 100).unwrap(),
            "a base started before the gap must not clear it"
        );
        assert!(gap_path(&root).exists());
        assert!(clear_gap(&root, 101).unwrap());
        assert!(!gap_path(&root).exists());
        // A marker replaced between check and clear survives.
        durable::write_json(&gap_path(&root), &json!({"detected_ms": 200})).unwrap();
        let seen = gap_closed_by(&root, 300).unwrap().unwrap();
        durable::write_json(&gap_path(&root), &json!({"detected_ms": 250})).unwrap();
        remove_gap(&root, seen).unwrap();
        assert_eq!(
            durable::read_json(&gap_path(&root)).unwrap()["detected_ms"],
            250
        );
        // A crash mid-clear leaves only the clearing file: health still sees a gap,
        // and the next check restores it unless a newer marker exists.
        fs::remove_file(gap_path(&root)).unwrap();
        let orphan = root.join("wal/.gap.clearing.1");
        durable::write_json(&orphan, &json!({"detected_ms": 400})).unwrap();
        assert!(gap_present(&root));
        assert_eq!(gap_closed_by(&root, 500).unwrap(), Some(400));
        assert!(gap_path(&root).exists() && !orphan.exists());
        assert_ne!(generation(&root).unwrap(), before);
        fs::remove_dir_all(&root).unwrap();
    }
}
