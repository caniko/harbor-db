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
