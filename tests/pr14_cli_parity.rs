//! Compare retained Python parsers with real Rust binaries, without storage access.
use std::{collections::BTreeSet, process::Command};

fn options(help: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(help)
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|word| word.starts_with("--") && word.len() > 2)
        .map(String::from)
        .collect()
}

fn compare(module: &str, binary: &str, prefix: &[&str], operations: &[&str]) {
    let mut global = BTreeSet::new();
    for operation in std::iter::once("").chain(operations.iter().copied()) {
        let mut args = prefix.to_vec();
        if !operation.is_empty() {
            args.push(operation);
        }
        args.push("--help");
        let old = Command::new("python3")
            .args(["-B", "-m", &format!("harbor_db.{module}")])
            .args(&args)
            .env("PYTHONPATH", "python")
            .output()
            .unwrap();
        let new = Command::new(binary).args(&args).output().unwrap();
        assert!(
            old.status.success(),
            "{module} {operation}: {}",
            String::from_utf8_lossy(&old.stderr)
        );
        assert!(
            new.status.success(),
            "{module} {operation}: {}",
            String::from_utf8_lossy(&new.stderr)
        );
        let mut old_options = options(&old.stdout);
        let mut new_options = options(&new.stdout);
        if operation.is_empty() {
            global = old_options.clone();
        }
        // Clap may repeat global options in a child help page; argparse lists
        // them only at the root. Compare the complete accepted flag surface.
        old_options.extend(global.iter().cloned());
        new_options.extend(global.iter().cloned());
        assert_eq!(
            old_options, new_options,
            "public options changed: {module} {operation}"
        );
    }
}

#[test]
fn full_pr14_operator_flag_surface_matches_retained_python_parsers() {
    let manifest = ["--config", "/nonexistent-pr14-parity-manifest"];
    compare(
        "postgres",
        env!("CARGO_BIN_EXE_harbor-db-postgres"),
        &manifest,
        &[
            "check",
            "adopt",
            "inspect-live",
            "adopt-live",
            "inspect-recovery",
            "fence-open",
            "fence-close",
            "inspect-offline-fence",
            "inhibit-startup",
            "release-startup",
            "inspect-fence",
            "prepare-recovery",
            "snapshot-records",
            "certify-recovery",
            "upgrade",
            "serve",
        ],
    );
    compare(
        "resource",
        env!("CARGO_BIN_EXE_harbor-db-resource"),
        &manifest,
        &["check", "adopt", "serve"],
    );
    compare(
        "application_backup",
        env!("CARGO_BIN_EXE_harbor-db-application-backup"),
        &manifest,
        &["capture", "inspect", "certify"],
    );
    compare(
        "application_transition",
        env!("CARGO_BIN_EXE_harbor-db-transition"),
        &manifest,
        &[
            "plan",
            "bind-candidate",
            "status",
            "prepare",
            "commit",
            "enable-writes",
            "complete",
            "abort",
            "retire",
        ],
    );
    compare(
        "cutover",
        env!("CARGO_BIN_EXE_harbor-db-cutover"),
        &[],
        &["check", "certify", "certify-worker", "serve"],
    );
    compare(
        "provision",
        env!("CARGO_BIN_EXE_harbor-db-provision"),
        &manifest,
        &[],
    );
    compare(
        "postgres_drill",
        env!("CARGO_BIN_EXE_harbor-db-postgres-drill"),
        &[],
        &[],
    );
    compare(
        "backup",
        env!("CARGO_BIN_EXE_harbor-db-backup-prune"),
        &[],
        &[],
    );
    compare(
        "durable",
        env!("CARGO_BIN_EXE_harbor-db-durable"),
        &[],
        &["write", "publish-tree", "publish-file"],
    );
    compare(
        "transition_manifest",
        env!("CARGO_BIN_EXE_harbor-db-transition-start"),
        &[],
        &[],
    );
}
