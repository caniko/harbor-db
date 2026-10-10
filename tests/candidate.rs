#![cfg(feature = "testing")]
use harbor_db::testing::candidate;
use std::{fs, process::Command};

#[test]
fn retained_candidate_contains_edits_and_new_sources_but_no_build_or_private_state() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::write(source.join("tracked"), "old").unwrap();
    fs::write(source.join(".gitignore"), "target/\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::write(source.join("tracked"), "candidate").unwrap();
    fs::write(source.join("new.rs"), "crystallized").unwrap();
    fs::write(source.join(".envrc"), "private-local-state").unwrap();
    fs::create_dir(source.join("target")).unwrap();
    fs::write(source.join("target/ignored"), "build").unwrap();
    fs::create_dir(source.join(".nix-results")).unwrap();
    std::os::unix::fs::symlink("/nix/store", source.join(".nix-results/result")).unwrap();
    let retained = candidate::retain(&source, &temp.path().join("retained")).unwrap();
    assert_eq!(
        fs::read_to_string(retained.source.join("tracked")).unwrap(),
        "candidate"
    );
    assert!(retained.source.join("new.rs").exists());
    assert!(!retained.source.join(".envrc").exists());
    assert!(!retained.source.join("target").exists());
    assert!(!retained.source.join(".nix-results").exists());
    candidate::verify(&retained).unwrap();
    fs::write(retained.source.join("tracked"), "changed").unwrap();
    assert!(candidate::verify(&retained).is_err());
}

#[test]
fn candidate_redirects_and_duplicate_publication_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&source)
        .status()
        .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", source.join("redirect")).unwrap();
    assert!(candidate::retain(&source, &temp.path().join("rejected")).is_err());
    fs::remove_file(source.join("redirect")).unwrap();
    fs::write(source.join("file"), "retained").unwrap();
    let destination = temp.path().join("retained");
    candidate::retain(&source, &destination).unwrap();
    assert!(candidate::retain(&source, &destination).is_err());
}

#[test]
fn generated_hook_link_is_excluded_with_present_or_absent_store_target() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::write(source.join("source.rs"), "candidate source").unwrap();
    let hook = source.join(".pre-commit-config.yaml");
    for (index, target) in [
        temp.path().join("generated-hook"),
        temp.path().join("absent-hook"),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 0 {
            fs::write(&target, "generated hook").unwrap();
        }
        std::os::unix::fs::symlink(target, &hook).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        let retained =
            candidate::retain(&source, &temp.path().join(format!("retained-{index}"))).unwrap();
        assert_eq!(
            fs::read(retained.source.join("source.rs")).unwrap(),
            b"candidate source"
        );
        assert!(
            retained
                .source
                .join(".pre-commit-config.yaml")
                .symlink_metadata()
                .is_err()
        );
        candidate::verify(&retained).unwrap();
        fs::remove_file(&hook).unwrap();
    }
}

#[test]
fn dangling_source_symlink_is_rejected_instead_of_silently_omitted() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::write(source.join("source.rs"), "candidate source").unwrap();
    std::os::unix::fs::symlink("absent", source.join("redirect.rs")).unwrap();
    assert!(candidate::retain(&source, &temp.path().join("rejected")).is_err());
}
