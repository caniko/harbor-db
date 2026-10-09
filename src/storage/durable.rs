//! Persistent inode leases and synchronized publication. Callers own semantics.
use super::{Result, codec, invalid};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    mem::ManuallyDrop,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn open_regular(path: &Path, writable: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("storage is not a regular file"));
    }
    Ok(file)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?
        .sync_all()?;
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("publication has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("publication has no filename"))?;
    let temporary = parent.join(format!(
        ".{}.{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = codec::encode(value, false)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

pub fn read_json(path: &Path) -> Result<Value> {
    let mut file = open_regular(path, false)?;
    read_json_file(&mut file)
}

fn read_json_file(file: &mut File) -> Result<Value> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Load an explicitly selected policy, including NixOS /etc aliases. This is
/// not a receipt reader: redirects are permitted only to canonical, root-owned,
/// read-only Nix store files. Direct regular files remain legacy fixture inputs.
/// The caller trusts the explicit policy selection, not arbitrary storage paths.
pub fn read_config_json(path: &Path) -> Result<Value> {
    let (_, mut file) = open_configuration(path)?;
    read_json_file(&mut file)
}

/// Resolve explicit configuration for workers that must retain its source path.
pub fn configuration_path(path: &Path) -> Result<PathBuf> {
    Ok(open_configuration(path)?.0)
}

/// Require immutable policy even when the explicit input is a regular file.
pub fn immutable_config_path(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    open_store_configuration(&canonical)?;
    Ok(canonical)
}

fn open_configuration(path: &Path) -> Result<(PathBuf, File)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = fs::canonicalize(&absolute)?;
    let mut redirected = false;
    for component in absolute.ancestors() {
        redirected |= fs::symlink_metadata(component)?.file_type().is_symlink();
    }
    let file = if redirected || canonical.starts_with("/nix/store") {
        open_store_configuration(&canonical)?
    } else {
        open_regular(&canonical, false)?
    };
    Ok((canonical, file))
}

/// Numeric ownership view of root for immutable testing inputs. Process
/// privilege checks must still use UID 0; this only models filesystem metadata.
pub(super) fn root_owner_uid() -> Result<u32> {
    #[cfg(feature = "testing")]
    {
        option_env!("HARBOR_DB_TEST_ROOT_UID")
            .unwrap_or("0")
            .parse::<u32>()
            .map_err(|_| invalid("invalid compile-time root ownership fixture"))
    }
    #[cfg(not(feature = "testing"))]
    {
        Ok(0)
    }
}

fn open_store_configuration(path: &Path) -> Result<File> {
    let root = Path::new("/nix/store");
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid("configuration alias escapes the immutable Nix store"))?;
    let output = relative
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| invalid("configuration lacks a Nix store output"))?;
    let bytes = output.as_bytes();
    if bytes.len() <= 33
        || bytes[32] != b'-'
        || !bytes[..32]
            .iter()
            .all(|b| b"0123456789abcdfghijklmnpqrsvwxyz".contains(b))
        || !bytes[33..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(b))
    {
        return Err(invalid(
            "configuration has an invalid Nix store output name",
        ));
    }
    // Nix's Cargo sandbox maps the host's root-owned input inodes to an
    // unmapped UID. Only a testing build may bind that numeric view at compile
    // time; no runtime environment or lifecycle manifest can change ownership.
    let root_uid = root_owner_uid()?;
    // Canonicalization resolves /etc's parent chains and store-internal links.
    // Only root can replace these read-only output ancestors. Open the resolved
    // target, never the alias, and check the actual descriptor's inode metadata.
    for parent in path.ancestors().skip(1).take_while(|p| *p != root) {
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir() || metadata.uid() != root_uid || metadata.mode() & 0o222 != 0 {
            return Err(invalid("configuration has a mutable Nix store ancestor"));
        }
    }
    let file = open_regular(path, false)?;
    let metadata = file.metadata()?;
    if metadata.uid() != root_uid || metadata.mode() & 0o222 != 0 {
        return Err(invalid(
            "configuration requires a root-owned read-only store file",
        ));
    }
    Ok(file)
}

pub fn sync_tree(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(invalid("publication tree is not a directory"));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            open_regular(&entry.path(), false)?.sync_all()?;
        } else {
            return Err(invalid(format!(
                "external or special storage is unsupported: {}",
                entry.path().display()
            )));
        }
    }
    sync_directory(path)
}

#[cfg(target_os = "linux")]
fn rename_new(source: &Path, destination: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| invalid("NUL in path"))?;
    let destination =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| invalid("NUL in path"))?;
    // SAFETY: both C strings remain live, and AT_FDCWD uses no borrowed descriptors.
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_new(_source: &Path, _destination: &Path) -> Result<()> {
    Err(invalid("storage publication requires Linux"))
}

pub fn publish_file(source: &Path, destination: &Path) -> Result<()> {
    open_regular(source, false)?.sync_all()?;
    publish(source, destination)
}

pub fn publish_tree(source: &Path, destination: &Path) -> Result<()> {
    sync_tree(source)?;
    publish(source, destination)
}

fn publish(source: &Path, destination: &Path) -> Result<()> {
    rename_new(source, destination)?;
    let from = source
        .parent()
        .ok_or_else(|| invalid("source has no parent"))?;
    let to = destination
        .parent()
        .ok_or_else(|| invalid("destination has no parent"))?;
    sync_directory(to)?;
    if from != to {
        sync_directory(from)?;
    }
    Ok(())
}

pub struct Lease(ManuallyDrop<File>);

impl Lease {
    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
    /// Enable legacy process-wide inheritance. Callers must exclude concurrent
    /// forks until exec or flag restoration; prefer `process::exec` for writers
    /// and `process::CommandSpec::leases` for explicitly selected workers.
    pub fn inherit(&self) -> Result<()> {
        super::process::inherit_fd(self.fd())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Wait for unrelated fork/exec handshakes before closing this descriptor.
        // Otherwise an unrelated child can retain our flock until its exec,
        // causing a false contention result after the owner's release.
        // A poisoned gate already prevents new coordinated spawns.
        let _fork = super::process::fork_guard().ok();
        // SAFETY: this is the only manual drop of the live File. Do not unlock
        // its open-file description: an explicitly inherited worker still owns it.
        unsafe {
            ManuallyDrop::drop(&mut self.0);
        }
    }
}

pub fn lock(path: &Path, shared: bool, create: bool) -> Result<Lease> {
    lock_anchor(path, shared, create, false)
}

/// The frozen Linux backup pruner also locks persistent FIFO inodes. Opening
/// these O_RDWR never waits for a stream peer; no data is read or written.
pub(crate) fn backup_lock(path: &Path) -> Result<Lease> {
    lock_anchor(path, false, true, true)
}

fn lock_anchor(path: &Path, shared: bool, create: bool, allow_fifo: bool) -> Result<Lease> {
    let _fork = super::process::fork_guard()?;
    use std::{
        ffi::CString,
        os::{
            fd::FromRawFd,
            unix::{ffi::OsStrExt, fs::FileTypeExt},
        },
    };
    let path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("NUL in lease path"))?;
    let flags = (if shared { libc::O_RDONLY } else { libc::O_RDWR })
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if create { libc::O_CREAT } else { 0 };
    // SAFETY: path is a live NUL-terminated string; a successful descriptor is
    // transferred exactly once into File. O_RDONLY|O_CREAT preserves legacy leases.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !(metadata.is_file() || allow_fifo && metadata.file_type().is_fifo()) {
        return Err(invalid("lease anchor is not a regular file"));
    }
    // SAFETY: file owns this valid descriptor through the syscall and lease lifetime.
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if shared { libc::LOCK_SH } else { libc::LOCK_EX }) | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Lease(ManuallyDrop::new(file)))
}
