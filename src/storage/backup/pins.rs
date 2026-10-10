//! Source-local finalizer intents protect their base and actual manifest WAL floor.
use super::super::{Result, codec, durable, invalid};
use std::{collections::BTreeSet, fs, io::Read, path::Path};

pub(super) struct Protection {
    pub backups: BTreeSet<String>,
    _lease: durable::Lease,
}

fn canonical(path: &Path) -> Result<()> {
    if !path.is_absolute() || fs::canonicalize(path)? != path {
        return Err(invalid("redirected source-local storage"));
    }
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            return Err(invalid("redirected source-local ancestor"));
        }
    }
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    canonical(path)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(invalid("source-local storage requires a directory"));
    }
    Ok(())
}

fn bytes(path: &Path) -> Result<Vec<u8>> {
    canonical(path)?;
    let file = durable::open_regular(path, false)?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(invalid("source-local metadata exceeds size limit"));
    }
    Ok(bytes)
}

fn child(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && !value.ends_with(".partial")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub(super) fn load(root: &Path, segment_bytes: u64) -> Result<Option<Protection>> {
    let recovery = root.join("recovery");
    match fs::symlink_metadata(&recovery) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    directory(root)?;
    directory(&recovery)?;
    if std::str::from_utf8(&bytes(&recovery.join("PROTOCOL"))?)
        .map_err(|_| invalid("invalid protocol encoding"))?
        .trim()
        != "source-local-v1"
    {
        return Err(invalid("invalid source-local protocol"));
    }
    let pins = recovery.join("pins");
    directory(&pins)?;
    directory(&root.join("locks"))?;
    canonical(&root.join("locks/mutate"))?;
    let lease = durable::lock(&root.join("locks/mutate"), false, false)?;
    directory(&root.join("base"))?;
    directory(&root.join("wal"))?;
    let mut backups = BTreeSet::new();
    for entry in fs::read_dir(&pins)? {
        let entry = entry?;
        let name = entry.file_name();
        let id = name
            .to_str()
            .and_then(|s| s.strip_suffix(".json"))
            .filter(|id| child(id) && !id.starts_with('.') && !id.contains(".partial"))
            .ok_or_else(|| invalid("uncertain source-local pin entry"))?;
        let pin = crate::storage::codec::decode(&bytes(&entry.path())?)?;
        let backup = pin["backup_id"]
            .as_str()
            .filter(|s| child(s))
            .ok_or_else(|| invalid("invalid pinned backup ID"))?;
        let digest = pin["manifest_sha256"]
            .as_str()
            .filter(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
            })
            .ok_or_else(|| invalid("invalid pinned manifest digest"))?;
        if pin["version"].as_u64() != Some(1)
            || pin["capture_id"].as_str() != Some(id)
            || pin["wal_segment_bytes"].as_u64() != Some(segment_bytes)
        {
            return Err(invalid("invalid source-local pin metadata"));
        }
        let base = root.join("base").join(backup);
        directory(&base)?;
        let manifest = base.join("backup_manifest");
        canonical(&manifest)?;
        if codec::file_digest(&manifest)? != digest {
            return Err(invalid("pinned manifest digest mismatch"));
        }
        backups.insert(backup.to_owned());
    }
    Ok(Some(Protection {
        backups,
        _lease: lease,
    }))
}
