#![cfg(feature = "testing")]
use harbor_db::testing::{
    evidence::{self, ArtifactKind, ArtifactSpec},
    supervisor::{self, CaseSpec, Execution, RunSpec, Verdict},
};
use std::{collections::BTreeMap, fs};

#[test]
fn nix_documents_require_one_bound_nonredirected_passing_export() {
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    for output in [&first, &second] {
        fs::create_dir(output).unwrap();
    }
    let spec = ArtifactSpec {
        source: "bound-case".into(),
        path: tmp.path().join("acceptance.json"),
        kind: ArtifactKind::Semantic,
        required: true,
        sha256: None,
    };
    let document = br#"{"schema":1,"case_id":"bound-case","assertions":[{"name":"native-contract","passed":true}]}"#;
    // Provenance retains both realized outputs and their derivations. The
    // derivation is a regular file, not an acceptance-document directory.
    let derivation = tmp.path().join("retained-source.drv");
    fs::write(&derivation, b"retained derivation").unwrap();
    let outputs = vec![derivation, first.clone(), second.clone()];
    assert!(evidence::import_nix_document(&spec, &outputs).is_err());
    fs::write(first.join("acceptance.json"), document).unwrap();
    evidence::import_nix_document(&spec, &outputs).unwrap();
    assert_eq!(fs::read(&spec.path).unwrap(), document);
    fs::write(second.join("acceptance.json"), document).unwrap();
    assert!(evidence::import_nix_document(&spec, &outputs).is_err());
    fs::remove_file(second.join("acceptance.json")).unwrap();
    for invalid in [
        String::from_utf8(document.to_vec())
            .unwrap()
            .replace("bound-case", "other-case"),
        String::from_utf8(document.to_vec())
            .unwrap()
            .replace("true", "false"),
    ] {
        fs::write(first.join("acceptance.json"), invalid).unwrap();
        assert!(evidence::import_nix_document(&spec, &outputs).is_err());
        assert_eq!(
            fs::read(&spec.path).unwrap(),
            document,
            "failed import replaced retained acceptance"
        );
    }
    fs::remove_file(first.join("acceptance.json")).unwrap();
    std::os::unix::fs::symlink(&spec.path, first.join("acceptance.json")).unwrap();
    assert!(evidence::import_nix_document(&spec, &outputs).is_err());
    fs::remove_file(first.join("acceptance.json")).unwrap();
    fs::write(first.join("acceptance.json"), document).unwrap();
    let mut wrong_hash = spec.clone();
    wrong_hash.sha256 = Some("0".repeat(64));
    assert!(evidence::import_nix_document(&wrong_hash, &outputs).is_err());
    let junit = ArtifactSpec {
        kind: ArtifactKind::Junit,
        path: tmp.path().join("case.xml"),
        ..spec
    };
    fs::write(
        first.join("junit.xml"),
        b"<testsuite tests=\"1\"><testcase name=\"native-VM\"/></testsuite>",
    )
    .unwrap();
    evidence::import_nix_document(&junit, &outputs).unwrap();
    assert_eq!(
        fs::read(&junit.path).unwrap(),
        fs::read(first.join("junit.xml")).unwrap()
    );
}

#[test]
fn artifacts_reject_symlinks_malformed_xml_and_changed_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let snapshot = tmp.path().join("snapshot");
    fs::create_dir(&snapshot).unwrap();
    let source = snapshot.join("input");
    fs::write(&source, "input").unwrap();
    let junit = tmp.path().join("junit.xml");
    fs::write(
        &junit,
        "<testsuite tests=\"1\"><testcase name=\"works\"/></testsuite>",
    )
    .unwrap();
    let artifact = ArtifactSpec {
        source: "case".into(),
        path: junit.clone(),
        kind: ArtifactKind::Junit,
        required: true,
        sha256: None,
    };
    let run = supervisor::create_run(Some(tmp.path()), "artifacts", RunSpec { schema: 1, source_root: snapshot, inputs: vec![supervisor::bind_file(&source).unwrap()], cases: vec![CaseSpec { id: "case".into(), execution: Execution::Argv { argv: vec!["sh".into(), "-c".into(), format!("printf '%s' '<testsuite tests=\"1\"><testcase name=\"works\"/></testsuite>' > '{}'", junit.display())], env: BTreeMap::new() }, deadline_seconds: 5, artifacts: vec![artifact.clone()], dependencies: vec![], resources: vec![], platform: "linux".into() }] }).unwrap();
    supervisor::worker(&run).unwrap();
    let verification = supervisor::verify(&run).unwrap();
    assert_eq!(
        verification.verdict,
        Verdict::Passed,
        "{:?}",
        verification.reasons
    );
    fs::write(&source, "changed").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    fs::write(&junit, "<testsuite><testcase").unwrap();
    assert!(evidence::capture(&run, 9, &artifact).is_err());
    fs::remove_file(&junit).unwrap();
    std::os::unix::fs::symlink(&source, &junit).unwrap();
    assert!(evidence::capture(&run, 10, &artifact).is_err());
}

#[test]
fn zero_exit_does_not_accept_missing_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let artifact = ArtifactSpec {
        source: "case".into(),
        path: tmp.path().join("missing"),
        kind: ArtifactKind::File,
        required: true,
        sha256: None,
    };
    assert!(evidence::capture(tmp.path(), 0, &artifact).is_err());
}

fn run_case(
    root: &std::path::Path,
    id: &str,
    document: Option<&str>,
    artifacts: bool,
) -> std::path::PathBuf {
    let snapshot = root.join("snapshot");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("input"), "immutable fixture source").unwrap();
    let output = root.join(format!("{id}.xml"));
    let mut env = BTreeMap::new();
    let command = if let Some(document) = document {
        env.insert("DOCUMENT".into(), document.into());
        env.insert("ARTIFACT".into(), output.display().to_string());
        "printf '%s' \"$DOCUMENT\" > \"$ARTIFACT\""
    } else {
        "true"
    };
    let spec = RunSpec {
        schema: 1,
        source_root: snapshot.clone(),
        inputs: supervisor::bind_tree(&snapshot).unwrap(),
        cases: vec![CaseSpec {
            id: "case".into(),
            execution: Execution::Argv {
                argv: vec!["sh".into(), "-c".into(), command.into()],
                env,
            },
            deadline_seconds: 5,
            artifacts: if artifacts {
                vec![ArtifactSpec {
                    source: "case".into(),
                    path: output,
                    kind: ArtifactKind::Junit,
                    required: true,
                    sha256: None,
                }]
            } else {
                vec![]
            },
            dependencies: vec![],
            resources: vec![],
            platform: "linux".into(),
        }],
    };
    let run = supervisor::create_run(Some(root), id, spec).unwrap();
    supervisor::worker(&run).unwrap();
    run
}

#[test]
fn zero_exit_requires_fresh_nonempty_passing_case_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    for (id, document, artifacts) in [
        ("bare", None, false),
        ("missing", None, true),
        ("empty", Some("<testsuite tests=\"0\"/>"), true),
        (
            "failure",
            Some("<testsuite><testcase name=\"bad\"><failure/></testcase></testsuite>"),
            true,
        ),
        (
            "skipped",
            Some("<testsuite><testcase name=\"skip\"><skipped/></testcase></testsuite>"),
            true,
        ),
        (
            "aggregate",
            Some(
                "<testsuites tests=\"2\"><testsuite><testcase name=\"one\"/></testsuite></testsuites>",
            ),
            true,
        ),
        ("malformed", Some("<testsuite>"), true),
        (
            "notrun",
            Some("<testsuite><testcase name=\"one\" status=\"notrun\"/></testsuite>"),
            true,
        ),
    ] {
        let run = run_case(tmp.path(), id, document, artifacts);
        let state = supervisor::status(&run).unwrap();
        assert_eq!(state.results[0].code, Some(0));
        assert_eq!(state.verification.verdict, Verdict::Failed, "{id}");
    }
    fs::write(
        tmp.path().join("stale.xml"),
        "<testsuite><testcase name=\"old\"/></testsuite>",
    )
    .unwrap();
    let run = run_case(tmp.path(), "stale", None, true);
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Failed);
    assert!(
        supervisor::status(&run).unwrap().results[0]
            .evidence_errors
            .iter()
            .any(|e| e.contains("not refreshed"))
    );
}

#[test]
fn retained_evidence_results_spec_and_corpus_are_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let passing = "<testsuite tests=\"1\"><testcase name=\"real\"/></testsuite>";
    let run = run_case(tmp.path(), "retained", Some(passing), true);
    assert_eq!(supervisor::verify(&run).unwrap().verdict, Verdict::Passed);
    fs::write(run.join("artifact-0.bin"), "tampered").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    let run = run_case(tmp.path(), "results", Some(passing), true);
    fs::write(run.join("results.json"), "[]").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    let run = run_case(tmp.path(), "terminal", Some(passing), true);
    fs::write(run.join("terminal.json"), "{bad").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    let run = run_case(tmp.path(), "spec", Some(passing), true);
    fs::write(run.join("spec.json"), "{}").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    let run = run_case(tmp.path(), "corpus", Some(passing), true);
    fs::write(tmp.path().join("snapshot/unbound-extra"), "extra source").unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
    fs::remove_file(tmp.path().join("snapshot/unbound-extra")).unwrap();
    fs::remove_dir_all(tmp.path().join("snapshot")).unwrap();
    assert_eq!(
        supervisor::verify(&run).unwrap().verdict,
        Verdict::Indeterminate
    );
}

#[test]
fn descriptor_traversal_rejects_intermediate_links_and_fifos() {
    let tmp = tempfile::tempdir().unwrap();
    let directory = tmp.path().join("real");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("file"), "data").unwrap();
    std::os::unix::fs::symlink(&directory, tmp.path().join("link")).unwrap();
    assert!(evidence::bounded_read(&tmp.path().join("link/file")).is_err());
    let fifo = tmp.path().join("fifo");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(evidence::bounded_read(&fifo).is_err());
}

#[test]
fn diagnostic_subprocesses_bound_time_output_and_suppress_stderr() {
    use std::time::{Duration, Instant};
    let began = Instant::now();
    let timeout = evidence::bounded_diagnostic(
        vec!["sh".into(), "-c".into(), "sleep 20".into()],
        Duration::from_millis(50),
    )
    .unwrap_err();
    assert!(timeout.to_string().contains("execution limit"));
    assert!(began.elapsed() < Duration::from_secs(2));
    let overflow = evidence::bounded_diagnostic(
        vec!["sh".into(), "-c".into(), "head -c 70000 /dev/zero".into()],
        Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(overflow.to_string().contains("size limit"));
    let exit = evidence::bounded_diagnostic(
        vec![
            "sh".into(),
            "-c".into(),
            "echo private-diagnostic >&2; exit 9".into(),
        ],
        Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(exit.to_string().contains('9'));
    assert!(!exit.to_string().contains("private-diagnostic"));
}
