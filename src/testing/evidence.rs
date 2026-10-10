//! Bounded, retained acceptance evidence. Exit success alone is never evidence.
use super::supervisor::{Result, error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    File,
    Junit,
    Semantic,
    NixOutputs,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSpec {
    pub source: String,
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub required: bool,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    pub source: String,
    pub original_path: PathBuf,
    pub retained_path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub junit_cases: Option<usize>,
}

/// Descriptor-relative traversal prevents intermediate symlink substitution.
pub(crate) fn open(path: &Path, flags: i32, mode: u32) -> Result<File> {
    if !path.is_absolute() {
        return Err(error("evidence paths must be absolute"));
    }
    let mut directory = File::open("/")?;
    let components: Vec<_> = path
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(error("path traversal is forbidden"));
        };
        let name = CString::new(name.as_bytes())?;
        let last = index + 1 == components.len();
        let flags = if last {
            flags
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY
        };
        // SAFETY: directory pins the parent inode; name remains live for openat.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if directory.metadata()?.file_type().is_symlink() {
            return Err(error("symlink evidence is forbidden"));
        }
    }
    Ok(directory)
}

pub fn check_path(path: &Path) -> Result<()> {
    open(path, libc::O_PATH, 0)?;
    Ok(())
}

static NEXT: AtomicU64 = AtomicU64::new(0);
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = open(
        path.parent().ok_or_else(|| error("missing parent"))?,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| error("missing filename"))?
            .as_bytes(),
    )?;
    let temporary = CString::new(format!(
        ".write-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let outcome = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        parent.sync_all()?;
        Ok(())
    })();
    if outcome.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    outcome
}

pub fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    let mut file = open(path, libc::O_RDONLY, 0)?;
    if !file.metadata()?.is_file() {
        return Err(error("evidence is not a regular file"));
    }
    if file.metadata()?.len() > MAX_ARTIFACT_BYTES {
        return Err(error("artifact exceeds capture limit"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(error("artifact grew beyond capture limit"));
    }
    Ok(bytes)
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Small diagnostic receipts are bounded independently of the original command.
/// Stderr is deliberately discarded; errors identify timeout/overflow/exit status.
pub fn bounded_diagnostic(argv: Vec<String>, timeout: std::time::Duration) -> Result<Vec<u8>> {
    let mut spec = crate::storage::process::CommandSpec::new(argv);
    spec.timeout = timeout;
    spec.maximum_output = 64 * 1024;
    let output = crate::storage::process::run(&spec)?;
    if !output.status.success() {
        return Err(error(format!(
            "diagnostic command exited with {} (stderr suppressed)",
            output.status
        )));
    }
    Ok(output.stdout)
}

pub(crate) fn nix_path_info(path: &Path) -> Result<serde_json::Value> {
    let bytes = bounded_diagnostic(
        vec![
            "nix".into(),
            "path-info".into(),
            "--json".into(),
            path.to_str()
                .ok_or_else(|| error("Nix path is not UTF-8"))?
                .into(),
        ],
        std::time::Duration::from_secs(10),
    )?;
    let info: serde_json::Value = serde_json::from_slice(&bytes)?;
    info.get(path.to_string_lossy().as_ref())
        .or_else(|| info.as_array().and_then(|a| a.first()))
        .cloned()
        .ok_or_else(|| error("Nix path-info record missing"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Semantic {
    schema: u32,
    case_id: String,
    assertions: Vec<Assertion>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assertion {
    name: String,
    passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixOutput {
    pub store_path: PathBuf,
    pub gc_root: PathBuf,
    pub nar_hash: String,
}

fn validate_nix(bytes: &[u8]) -> Result<()> {
    let outputs: Vec<NixOutput> = serde_json::from_slice(bytes)?;
    if outputs.is_empty() {
        return Err(error("Nix output manifest is empty"));
    }
    for output in outputs {
        if output.store_path.parent() != Some(Path::new("/nix/store"))
            || !output.nar_hash.starts_with("sha256-")
        {
            return Err(error("invalid Nix output provenance"));
        }
        check_path(&output.store_path)?;
        let parent = open(
            output
                .gc_root
                .parent()
                .ok_or_else(|| error("GC root parent missing"))?,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
        let name = CString::new(
            output
                .gc_root
                .file_name()
                .ok_or_else(|| error("GC root name missing"))?
                .as_bytes(),
        )?;
        let mut target = vec![0u8; 4096];
        let size = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        target.truncate(size as usize);
        if target != output.store_path.as_os_str().as_bytes() {
            return Err(error("Nix GC root binding mismatch"));
        }
        let registered = std::fs::read_dir("/nix/var/nix/gcroots/auto")?
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                std::fs::read_link(entry.path()).is_ok_and(|target| target == output.gc_root)
            });
        if !registered {
            return Err(error(
                "Nix output link is not registered as an indirect GC root",
            ));
        }
        let record = nix_path_info(&output.store_path)?;
        if record.get("narHash").and_then(|v| v.as_str()) != Some(output.nar_hash.as_str()) {
            return Err(error("Nix NAR hash binding mismatch"));
        }
    }
    Ok(())
}

fn validate_contents(spec: &ArtifactSpec, bytes: &[u8]) -> Result<Option<usize>> {
    match spec.kind {
        ArtifactKind::File => Ok(None),
        ArtifactKind::Junit => Ok(Some(junit(bytes)?)),
        ArtifactKind::Semantic => {
            let receipt: Semantic = serde_json::from_slice(bytes)?;
            let mut names = std::collections::BTreeSet::new();
            if receipt.schema != 1
                || receipt.case_id != spec.source
                || receipt.assertions.is_empty()
                || receipt
                    .assertions
                    .iter()
                    .any(|a| a.name.is_empty() || !a.passed || !names.insert(&a.name))
            {
                return Err(error("semantic assertions do not qualify case"));
            }
            Ok(None)
        }
        ArtifactKind::NixOutputs => {
            validate_nix(bytes)?;
            Ok(None)
        }
    }
}

fn junit(bytes: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(bytes)?;
    let document = roxmltree::Document::parse(text)?;
    let root = document.root_element();
    if !matches!(root.tag_name().name(), "testsuite" | "testsuites") {
        return Err(error("JUnit root is invalid"));
    }
    let mut cases = 0;
    for node in root.descendants().filter(|n| n.is_element()) {
        if matches!(node.tag_name().name(), "failure" | "error" | "skipped") {
            return Err(error("JUnit contains failed, errored or skipped cases"));
        }
        if node.tag_name().name() == "testcase" {
            if node.attribute("name").is_none_or(str::is_empty) {
                return Err(error("JUnit case has no name"));
            }
            cases += 1;
        }
        for count in ["failures", "errors", "skipped", "disabled"] {
            if let Some(value) = node.attribute(count)
                && value.parse::<usize>()? != 0
            {
                return Err(error("JUnit aggregate reports non-passing cases"));
            }
        }
        if node.tag_name().name() == "testcase"
            && (!node
                .parent_element()
                .is_some_and(|p| p.has_tag_name("testsuite"))
                || node
                    .attribute("status")
                    .is_some_and(|s| !matches!(s, "run" | "passed" | "completed")))
        {
            return Err(error("JUnit case structure or execution status is invalid"));
        }
        if matches!(node.tag_name().name(), "testsuite" | "testsuites")
            && let Some(count) = node.attribute("tests")
        {
            let actual = node
                .descendants()
                .filter(|n| n.has_tag_name("testcase"))
                .count();
            if count.parse::<usize>()? != actual {
                return Err(error("JUnit test count mismatch"));
            }
        }
    }
    if cases == 0 {
        return Err(error("JUnit contains no cases"));
    }
    Ok(cases)
}

pub fn capture(run: &Path, index: usize, spec: &ArtifactSpec) -> Result<ArtifactReceipt> {
    let bytes = bounded_read(&spec.path)?;
    let sha256 = hash(&bytes);
    if spec
        .sha256
        .as_ref()
        .is_some_and(|expected| expected != &sha256)
    {
        return Err(error("required artifact hash mismatch"));
    }
    let junit_cases = validate_contents(spec, &bytes)?;
    let retained_path = PathBuf::from(format!("artifact-{index}.bin"));
    atomic_write(&run.join(&retained_path), &bytes)?;
    Ok(ArtifactReceipt {
        source: spec.source.clone(),
        original_path: spec.path.clone(),
        retained_path,
        sha256,
        bytes: bytes.len() as u64,
        junit_cases,
    })
}

/// Import a declared acceptance document from exactly one realized Nix output.
/// Callers retain and verify output/source provenance before supplying outputs.
/// JUnit uses its established export name; semantic receipts use the declared
/// destination basename. Contents and case identity are validated without
/// rewriting the immutable producer's bytes.
pub fn import_nix_document(spec: &ArtifactSpec, outputs: &[PathBuf]) -> Result<()> {
    let name = match spec.kind {
        ArtifactKind::Junit => std::ffi::OsStr::new("junit.xml"),
        ArtifactKind::Semantic => spec
            .path
            .file_name()
            .ok_or_else(|| error("Nix semantic artifact has no export filename"))?,
        _ => {
            return Err(error(
                "Nix acceptance import requires JUnit or semantic evidence",
            ));
        }
    };
    let mut candidates = Vec::new();
    for output in outputs {
        let metadata = fs::symlink_metadata(output)?;
        // Retaining derivations preserves the source/input graph. Regular
        // store files cannot export child documents; inspect directories only.
        if metadata.is_file() {
            continue;
        }
        if !metadata.is_dir() {
            return Err(error(
                "Nix acceptance output is not a regular file or directory",
            ));
        }
        let path = output.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => candidates.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if candidates.len() != 1 {
        return Err(error(
            "Nix build must retain exactly one declared acceptance document",
        ));
    }
    let bytes = bounded_read(&candidates[0])?;
    validate_contents(spec, &bytes)?;
    if spec.sha256.as_ref().is_some_and(|sha| sha != &hash(&bytes)) {
        return Err(error("required artifact hash mismatch"));
    }
    atomic_write(&spec.path, &bytes)
}

pub fn validate(run: &Path, spec: &ArtifactSpec, receipt: &ArtifactReceipt) -> Result<()> {
    if receipt.source != spec.source
        || receipt.original_path != spec.path
        || receipt.retained_path.components().count() != 1
        || !matches!(
            receipt.retained_path.components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(error("artifact binding mismatch"));
    }
    let bytes = bounded_read(&run.join(&receipt.retained_path))?;
    let digest = hash(&bytes);
    if digest != receipt.sha256
        || bytes.len() as u64 != receipt.bytes
        || spec
            .sha256
            .as_ref()
            .is_some_and(|expected| expected != &digest)
    {
        return Err(error("retained artifact integrity mismatch"));
    }
    if validate_contents(spec, &bytes)? != receipt.junit_cases {
        return Err(error("artifact semantic receipt mismatch"));
    }
    Ok(())
}
