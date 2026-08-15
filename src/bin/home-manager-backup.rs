//! Standalone Home Manager collision-backup helper.
//!
//! Contract: takes the colliding target path as the only argument, reads the
//! backup extension from `HOME_MANAGER_BACKUP_EXT`, moves the target to
//! `target.<ext>`, and archives a stale backup to a unique timestamped name.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use chrono::Utc;
use thiserror::Error;

#[derive(Debug, Error)]
enum BackupError {
    #[error("HOME_MANAGER_BACKUP_EXT is not set by Home Manager")]
    MissingExtension,
    #[error("HOME_MANAGER_BACKUP_EXT must be a non-empty file-name suffix")]
    InvalidExtension,
    #[error("Home Manager collision does not exist: {0}")]
    MissingTarget(PathBuf),
    #[error("preserving stale Home Manager backup {0} as {1}: {2}")]
    ArchiveStale(PathBuf, PathBuf, std::io::Error),
    #[error("backing up Home Manager collision {0} as {1}: {2}")]
    MoveTarget(PathBuf, PathBuf, std::io::Error),
}

fn main() -> ExitCode {
    let mut args = env::args_os();
    let _program = args.next();
    let target = match (args.next(), args.next()) {
        (Some(target), None) => PathBuf::from(target),
        _ => {
            eprintln!("home-manager-backup: usage: home-manager-backup <target>");
            return ExitCode::from(2);
        }
    };
    match run(&target) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("home-manager-backup: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(target: &Path) -> Result<(), BackupError> {
    let extension =
        env::var("HOME_MANAGER_BACKUP_EXT").map_err(|_| BackupError::MissingExtension)?;
    backup(
        target,
        &extension,
        &Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
    )
}

fn backup(target: &Path, extension: &str, stamp: &str) -> Result<(), BackupError> {
    if extension.is_empty() || extension.contains('/') {
        return Err(BackupError::InvalidExtension);
    }
    fs::symlink_metadata(target).map_err(|_| BackupError::MissingTarget(target.to_owned()))?;
    let backup = append_suffix(target, &format!(".{extension}"));
    if fs::symlink_metadata(&backup).is_ok() {
        let archive = unique_archive_path(&backup, stamp);
        fs::rename(&backup, &archive)
            .map_err(|error| BackupError::ArchiveStale(backup.clone(), archive.clone(), error))?;
        eprintln!(
            "home-manager-backup: moved stale Home Manager backup {} to {}",
            backup.display(),
            archive.display()
        );
    }
    fs::rename(target, &backup)
        .map_err(|error| BackupError::MoveTarget(target.to_owned(), backup, error))
}

fn unique_archive_path(backup: &Path, stamp: &str) -> PathBuf {
    let base = append_suffix(backup, &format!(".canix-{stamp}"));
    if fs::symlink_metadata(&base).is_err() {
        return base;
    }
    (1..)
        .map(|index| append_suffix(&base, &format!("-{index}")))
        .find(|candidate| fs::symlink_metadata(candidate).is_err())
        .expect("unbounded archive suffix search")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(OsStr::new(suffix));
    value.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;

    fn tempdir(name: &str) -> PathBuf {
        let directory =
            env::temp_dir().join(format!("db-harbor-hm-backup-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn preserves_stale_backup_before_replacing_it() {
        let directory = tempdir("preserves-stale");
        let target = directory.join("settings.json");
        fs::write(&target, "current").unwrap();
        fs::write(directory.join("settings.json.bak"), "previous").unwrap();

        backup(&target, "bak", "20260803T200000Z").unwrap();

        assert_eq!(
            fs::read_to_string(directory.join("settings.json.bak")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(directory.join("settings.json.bak.canix-20260803T200000Z")).unwrap(),
            "previous"
        );
    }

    #[test]
    fn missing_target_is_an_error() {
        let directory = tempdir("missing-target");
        let result = backup(&directory.join("absent.json"), "bak", "20260803T200000Z");
        assert!(matches!(result, Err(BackupError::MissingTarget(_))));
    }

    #[test]
    fn invalid_extensions_are_rejected() {
        let directory = tempdir("invalid-extension");
        let target = directory.join("settings.json");
        fs::write(&target, "current").unwrap();
        assert!(matches!(
            backup(&target, "", "20260803T200000Z"),
            Err(BackupError::InvalidExtension)
        ));
        assert!(matches!(
            backup(&target, "ba/k", "20260803T200000Z"),
            Err(BackupError::InvalidExtension)
        ));
    }

    #[test]
    fn duplicate_archive_names_get_a_suffix() {
        let directory = tempdir("duplicate-archive");
        let target = directory.join("settings.json");
        fs::write(&target, "current").unwrap();
        fs::write(directory.join("settings.json.bak"), "previous").unwrap();
        fs::write(
            directory.join("settings.json.bak.canix-20260803T200000Z"),
            "older",
        )
        .unwrap();

        backup(&target, "bak", "20260803T200000Z").unwrap();

        assert_eq!(
            fs::read_to_string(directory.join("settings.json.bak.canix-20260803T200000Z-1"))
                .unwrap(),
            "previous"
        );
        assert_eq!(
            fs::read_to_string(directory.join("settings.json.bak.canix-20260803T200000Z")).unwrap(),
            "older"
        );
    }
}
