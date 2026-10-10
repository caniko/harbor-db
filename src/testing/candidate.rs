//! Immutable-by-binding snapshots outside source checkouts, including uncommitted sources.
use super::{
    evidence,
    supervisor::{self, FileBinding, Result},
};
use crate::storage::{codec, durable};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub version: u32,
    pub source: PathBuf,
    pub manifest: FileBinding,
    pub files: Vec<FileBinding>,
}

fn selected(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(n) if n == "target" || n == "__pycache__" || n == ".git" || n == ".direnv" || n == ".nix-results")) &&
        path != Path::new(".pre-commit-config.yaml") &&
        path.file_name().is_some_and(|n| n != ".envrc") && path.extension().is_none_or(|e| e != "pyc")
}

pub fn retain(source: &Path, destination: &Path) -> Result<Candidate> {
    evidence::check_path(source)?;
    if destination.starts_with(source) {
        return Err(supervisor::error(
            "candidate retention must be outside its checkout",
        ));
    }
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(source)
        .output()?;
    if !output.status.success() {
        return Err(supervisor::error("candidate source inventory failed"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| supervisor::error("candidate has no parent"))?;
    evidence::check_path(parent)?;
    fs::DirBuilder::new().mode(0o700).create(destination)?;
    let retained = destination.join("source");
    fs::DirBuilder::new().mode(0o700).create(&retained)?;
    let mut names = std::collections::BTreeSet::new();
    for bytes in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        use std::os::unix::ffi::OsStrExt;
        let name = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        if name.is_absolute()
            || name
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(supervisor::error("invalid candidate source path"));
        }
        if selected(&name) {
            match fs::symlink_metadata(source.join(&name)) {
                Ok(_) => {
                    names.insert(name);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut files = Vec::new();
    let mut manifest = Vec::new();
    for name in names {
        let original = source.join(&name);
        evidence::check_path(&original)?;
        let metadata = fs::symlink_metadata(&original)?;
        if !metadata.is_file() {
            return Err(supervisor::error("candidate contains non-file source"));
        }
        let bytes = evidence::bounded_read(&original)?;
        let path = retained.join(&name);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(
            path.parent()
                .ok_or_else(|| supervisor::error("source parent absent"))?,
        )?;
        durable::atomic_write(&path, &bytes)?;
        // Preserve only the executable bit; retained files are private to their owner.
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if metadata.permissions().mode() & 0o111 != 0 {
                0o700
            } else {
                0o600
            }),
        )?;
        let binding = FileBinding {
            path,
            sha256: codec::digest(&bytes),
        };
        manifest.push(serde_json::json!({"path": name, "sha256": binding.sha256, "executable": metadata.permissions().mode() & 0o111 != 0}));
        files.push(binding);
    }
    if files.is_empty() {
        return Err(supervisor::error("candidate source inventory is empty"));
    }
    let path = destination.join("sources.json");
    durable::write_json(&path, &serde_json::json!({"version":1,"files":manifest}))?;
    let candidate = Candidate {
        version: 1,
        source: retained,
        manifest: supervisor::bind_file(&path)?,
        files,
    };
    durable::write_json(
        &destination.join("candidate.json"),
        &serde_json::to_value(&candidate)?,
    )?;
    durable::sync_tree(destination)?;
    Ok(candidate)
}

pub fn verify(candidate: &Candidate) -> Result<()> {
    if candidate.version != 1 || candidate.files.is_empty() {
        return Err(supervisor::error("invalid retained candidate"));
    }
    for binding in std::iter::once(&candidate.manifest).chain(&candidate.files) {
        if supervisor::bind_file(&binding.path)?.sha256 != binding.sha256 {
            return Err(supervisor::error("retained source binding changed"));
        }
    }
    Ok(())
}
