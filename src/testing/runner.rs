//! Candidate-bound catalog execution and execution-backed coverage verification.
use super::{
    candidate,
    catalog::{self, Profile, Suite},
    evidence::{self, ArtifactKind, ArtifactSpec},
    executor::ExecutorSpec,
    supervisor::{self, CaseSpec, Execution, Result, RunSpec, Verdict, Verification},
};
use crate::storage::durable;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Hash the entire original identity, avoiding punctuation and truncation collisions.
pub fn safe_identity(id: &str) -> String {
    format!("case-{}", evidence::hash(id.as_bytes()))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    suite: PathBuf,
    profile: Profile,
    mapping: BTreeMap<String, String>,
    cases_sha256: String,
}

// Versioned helper retained outside the checkout and bound as an execution input.
// unittest's result API, not arbitrary logs or exit status, supplies assertions.
const UNITTEST_V1: &str = r#"import json, os, sys, unittest
selector, case_id, output = sys.argv[1:]
suite = unittest.defaultTestLoader.loadTestsFromName(selector)
class Result(unittest.TextTestResult):
    def __init__(self, *args):
        super().__init__(*args)
        self.successes = []
    def addSuccess(self, test):
        super().addSuccess(test)
        self.successes.append(test.id())
result = unittest.TextTestRunner(verbosity=2, resultclass=Result).run(suite)
passed = (result.testsRun == 1 and result.wasSuccessful() and not result.skipped
          and not result.expectedFailures and not result.unexpectedSuccesses
          and result.successes == [selector])
receipt = {'schema': 1, 'case_id': case_id, 'assertions': [
    {'name': 'unittest exact selector ' + selector, 'passed': passed}]}
with open(output, 'x', encoding='utf-8') as handle:
    json.dump(receipt, handle)
    handle.flush()
    os.fsync(handle.fileno())
sys.exit(0 if passed else 1)
"#;

fn directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    evidence::check_path(path)
}

/// Retain sources, helper, catalog and selection before creating the durable run.
/// Unsupported-platform cases remain selected and produce honest incomplete receipts.
pub fn create(
    suite_path: &Path,
    source: &Path,
    base: &Path,
    id: &str,
    profile: Profile,
) -> Result<PathBuf> {
    create_with_executor(suite_path, source, base, id, profile, None)
}

/// An explicit CLI executable enables registered Cargo and PostgreSQL adapters.
/// The executable and every adapter specification are retained, input-bound files.
pub fn create_with_executor(
    suite_path: &Path,
    source: &Path,
    base: &Path,
    id: &str,
    profile: Profile,
    executable: Option<&Path>,
) -> Result<PathBuf> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(supervisor::error("invalid run ID"));
    }
    let source = source.canonicalize()?;
    if !base.is_absolute() {
        return Err(supervisor::error("run base must be absolute"));
    }
    if base.starts_with(&source) {
        return Err(supervisor::error(
            "run storage must be outside the checkout",
        ));
    }
    directory(base)?;
    let base = base.canonicalize()?;
    if base.starts_with(&source) {
        return Err(supervisor::error(
            "run storage must be outside the checkout",
        ));
    }
    let bytes = evidence::bounded_read(&suite_path.canonicalize()?)?;
    let suite = Suite::parse(std::str::from_utf8(&bytes)?)?;
    let retained = candidate::retain(&source, &base.join(format!("{id}-candidate")))?;
    let work = base.join(format!("{id}-artifacts"));
    fs::DirBuilder::new().mode(0o700).create(&work)?;
    let catalog = work.join("catalog.toml");
    durable::atomic_write(&catalog, &bytes)?;
    let helper = work.join("unittest-v1.py");
    durable::atomic_write(&helper, UNITTEST_V1.as_bytes())?;
    let selected = suite.select(profile);
    let mapping: BTreeMap<_, _> = selected
        .iter()
        .map(|case| (case.id.clone(), safe_identity(&case.id)))
        .collect();
    let mut inputs = retained.files;
    inputs.push(retained.manifest);
    inputs.push(supervisor::bind_file(&catalog)?);
    inputs.push(supervisor::bind_file(&helper)?);
    let executor = if let Some(executable) = executable {
        if !executable.is_absolute() || fs::metadata(executable)?.permissions().mode() & 0o111 == 0
        {
            return Err(supervisor::error(
                "executor must be an absolute executable regular file",
            ));
        }
        let path = work.join("harbor-db-test");
        durable::atomic_write(&path, &evidence::bounded_read(executable)?)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        inputs.push(supervisor::bind_file(&path)?);
        Some(path)
    } else {
        None
    };
    let mut cases = Vec::new();
    let platform = format!("{}-linux", std::env::consts::ARCH);
    for case in selected {
        let identity = mapping[&case.id].clone();
        let workspace = work.join(&identity);
        fs::DirBuilder::new().mode(0o700).create(&workspace)?;
        let mut artifacts = case.resolved_artifacts(&workspace)?;
        for artifact in &mut artifacts {
            artifact.source = identity.clone();
            directory(
                artifact
                    .path
                    .parent()
                    .ok_or_else(|| supervisor::error("artifact parent missing"))?,
            )?;
        }
        let execution = match &case.execution {
            catalog::Execution::Nix { installable } => Execution::Nix {
                installable: installable.clone(),
            },
            catalog::Execution::Argv { argv, env } => {
                let mut argv = argv.clone();
                let mut env = env.clone();
                // Detached services do not inherit the invoking dev shell.
                for key in [
                    "PATH",
                    "CARGO_HOME",
                    "RUSTFLAGS",
                    "HARBOR_DB_TEST_POSTGRES",
                    "HARBOR_DB_TEST_POSTGRES_17",
                ] {
                    if let Ok(value) = std::env::var(key) {
                        env.entry(key.into()).or_insert(value);
                    }
                }
                env.insert("HARBOR_DB_TEST_CASE_ID".into(), identity.clone());
                env.entry("CARGO_BUILD_JOBS".into())
                    .or_insert_with(|| case.resources.cpu.to_string());
                env.insert(
                    "HARBOR_DB_TEST_ARTIFACT_DIR".into(),
                    workspace.to_string_lossy().into(),
                );
                env.insert(
                    "CARGO_TARGET_DIR".into(),
                    base.join(format!("{id}-build")).to_string_lossy().into(),
                );
                env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
                env.insert(
                    "PYTHONPATH".into(),
                    format!(
                        "{}:{}",
                        retained.source.join("python").display(),
                        retained.source.join("tests").display()
                    ),
                );
                if let Some(selector) = &case.python_migration_id {
                    if argv.len() != 5
                        || argv[1..4] != ["-B", "-m", "unittest"]
                        || argv[4] != *selector
                    {
                        return Err(supervisor::error(
                            "unsupported unittest argv; explicit artifacts required",
                        ));
                    }
                    let output = workspace.join("unittest-result.json");
                    argv = vec![
                        argv[0].clone(),
                        "-B".into(),
                        helper.to_string_lossy().into(),
                        selector.clone(),
                        identity.clone(),
                        output.to_string_lossy().into(),
                    ];
                    artifacts.push(ArtifactSpec {
                        source: identity.clone(),
                        path: output,
                        kind: ArtifactKind::Semantic,
                        required: true,
                        sha256: None,
                    });
                } else if let Some(executable) = &executor {
                    let selector = cargo_selector(&argv)?;
                    let prerequisite = match case.id.as_str() {
                        "prerequisite.disposable-postgres" => Some(("HARBOR_DB_TEST_POSTGRES", 18)),
                        "prerequisite.disposable-postgres-17" => {
                            Some(("HARBOR_DB_TEST_POSTGRES_17", 17))
                        }
                        _ => None,
                    }
                    .map(|(key, major)| {
                        let package = PathBuf::from(env.get(key).ok_or_else(|| {
                            supervisor::error(format!(
                                "{key} must identify an explicit disposable package"
                            ))
                        })?);
                        if !package.is_absolute() {
                            return Err(supervisor::error("disposable package must be absolute"));
                        }
                        Ok((package, major))
                    })
                    .transpose()?;
                    if selector.is_some() || prerequisite.is_some() {
                        let spec = ExecutorSpec {
                            schema: 1,
                            case_id: identity.clone(),
                            argv,
                            env,
                            workspace: workspace.clone(),
                            selector,
                            prerequisite,
                        };
                        let path = workspace.join("executor.json");
                        durable::write_json(&path, &serde_json::to_value(&spec)?)?;
                        inputs.push(supervisor::bind_file(&path)?);
                        let acceptance = workspace.join("acceptance.json");
                        if let Some(artifact) = artifacts.iter_mut().find(|a| a.path == acceptance)
                        {
                            if !matches!(artifact.kind, ArtifactKind::Semantic) {
                                return Err(supervisor::error(
                                    "executor acceptance must be semantic",
                                ));
                            }
                            artifact.required = true;
                        } else {
                            artifacts.push(ArtifactSpec {
                                source: identity.clone(),
                                path: acceptance,
                                kind: ArtifactKind::Semantic,
                                required: true,
                                sha256: None,
                            });
                        }
                        argv = vec![
                            executable.to_string_lossy().into(),
                            "execute".into(),
                            "--spec".into(),
                            path.to_string_lossy().into(),
                        ];
                        env = BTreeMap::new();
                    }
                }
                // Unregistered commands still require explicit acceptance producers.
                if !artifacts
                    .iter()
                    .any(|a| a.required && !matches!(a.kind, ArtifactKind::File))
                {
                    artifacts.push(ArtifactSpec {
                        source: identity.clone(),
                        path: workspace.join("acceptance.json"),
                        kind: ArtifactKind::Semantic,
                        required: true,
                        sha256: None,
                    });
                }
                Execution::Argv { argv, env }
            }
        };
        cases.push(CaseSpec {
            id: identity,
            execution,
            deadline_seconds: case.deadline_seconds,
            artifacts,
            dependencies: case
                .depends_on
                .iter()
                .map(|id| mapping[id].clone())
                .collect(),
            resources: case
                .resources
                .exclusive
                .iter()
                .map(|r| safe_identity(r))
                .collect(),
            platform: if case.platforms.contains(&platform) {
                platform.clone()
            } else {
                case.platforms[0].clone()
            },
        });
    }
    let binding = Binding {
        version: 1,
        suite: catalog,
        profile,
        mapping,
        cases_sha256: evidence::hash(&serde_json::to_vec(&cases)?),
    };
    let path = work.join("runner-binding.json");
    durable::write_json(&path, &serde_json::to_value(binding)?)?;
    inputs.push(supervisor::bind_file(&path)?);
    let run = supervisor::create_run(
        Some(&base),
        id,
        RunSpec {
            schema: 1,
            source_root: retained.source,
            inputs,
            cases,
        },
    )?;
    // The supervisor's verdict covers execution only. Suppress its desktop terminal
    // notice for catalog runs; CLI observers publish a coverage-qualified notice.
    evidence::atomic_write(
        &run.join("terminal-notification.json"),
        &serde_json::to_vec(&serde_json::json!({
            "event": "runner_owns_qualification_notification", "qualification": "pending"
        }))?,
    )?;
    Ok(run)
}

fn cargo_selector(argv: &[String]) -> Result<Option<String>> {
    if argv
        .first()
        .is_none_or(|a| Path::new(a).file_name().is_none_or(|name| name != "cargo"))
    {
        return Ok(None);
    }
    let separator = argv.iter().position(|a| a == "--");
    if argv.get(1).is_none_or(|a| a != "test")
        || separator.is_none_or(|index| {
            index < 3
                || index + 2 != argv.len()
                || argv[index + 1] != "--exact"
                || argv[index - 1].is_empty()
                || argv[index - 1].starts_with('-')
        })
    {
        return Err(supervisor::error(
            "unregistered Cargo shape: expected cargo test OPTIONS SELECTOR -- --exact",
        ));
    }
    Ok(separator.map(|index| argv[index - 1].clone()))
}

/// Require every applicable coverage cell to have a passed, artifact-verified case.
/// RunSpecs without a retained catalog expose execution verdicts only.
pub fn verify(run: &Path) -> Result<Verification> {
    let mut verification = supervisor::verify(run)?;
    let spec: RunSpec = serde_json::from_slice(&evidence::bounded_read(&run.join("spec.json"))?)?;
    let bindings: Vec<_> = spec
        .inputs
        .iter()
        .filter(|input| {
            input
                .path
                .file_name()
                .is_some_and(|n| n == "runner-binding.json")
        })
        .collect();
    if bindings.is_empty() {
        return Ok(verification);
    }
    if bindings.len() != 1 {
        return Err(supervisor::error("ambiguous runner binding"));
    }
    let bytes = evidence::bounded_read(&bindings[0].path)?;
    if evidence::hash(&bytes) != bindings[0].sha256 {
        return Err(supervisor::error("runner binding changed"));
    }
    let binding: Binding = serde_json::from_slice(&bytes)?;
    if binding.version != 1
        || binding.cases_sha256 != evidence::hash(&serde_json::to_vec(&spec.cases)?)
    {
        return Err(supervisor::error(
            "runner case specification binding changed",
        ));
    }
    if !spec.inputs.iter().any(|input| {
        input.path == binding.suite
            && supervisor::bind_file(&input.path)
                .is_ok_and(|current| current.sha256 == input.sha256)
    }) {
        return Err(supervisor::error(
            "retained catalog binding missing or changed",
        ));
    }
    let suite = Suite::load(&binding.suite)?;
    let expected: BTreeMap<_, _> = suite
        .select(binding.profile)
        .iter()
        .map(|case| (case.id.clone(), safe_identity(&case.id)))
        .collect();
    if expected != binding.mapping
        || spec.cases.len() != expected.len()
        || spec
            .cases
            .iter()
            .any(|c| !expected.values().any(|id| id == &c.id))
    {
        return Err(supervisor::error(
            "catalog selection differs from run specification",
        ));
    }
    let status = supervisor::status(run)?;
    for cell in suite.coverage(binding.profile).cells {
        if matches!(cell.status, catalog::CoverageStatus::NotApplicable { .. }) {
            continue;
        }
        let passed = cell.cases.iter().any(|original| {
            let id = &binding.mapping[original];
            let Some(case) = spec.cases.iter().find(|c| &c.id == id) else {
                return false;
            };
            let Some(result) = status.results.iter().find(|r| &r.id == id) else {
                return false;
            };
            result.reason == supervisor::ExitReason::Exited
                && result.code == Some(0)
                && result.signal.is_none()
                && result.evidence_errors.is_empty()
                && case
                    .artifacts
                    .iter()
                    .any(|a| a.required && !matches!(a.kind, ArtifactKind::File))
                && case
                    .artifacts
                    .iter()
                    .filter(|a| a.required)
                    .all(|artifact| {
                        result.artifacts.iter().any(|receipt| {
                            receipt.original_path == artifact.path
                                && evidence::validate(run, artifact, receipt).is_ok()
                        })
                    })
        });
        if !passed {
            if verification.verdict == Verdict::Passed {
                verification.verdict = Verdict::Incomplete;
            }
            verification.reasons.push(format!(
                "coverage {} / {} / {:?}: no passed case with retained acceptance evidence",
                cell.capability, cell.operation, cell.dimension
            ));
        }
    }
    Ok(verification)
}

pub fn status(run: &Path) -> Result<supervisor::RunStatus> {
    let mut status = supervisor::status(run)?;
    status.verification = verify(run)?;
    Ok(status)
}

/// Publish the current coverage-aware verdict independently of execution receipts.
/// An observer service returns successfully even when qualification fails: it must
/// not enter systemd's restart-on-failure loop for an ordinary negative verdict.
pub fn publish_verification(run: &Path, notify: bool) -> Result<supervisor::RunStatus> {
    let state = status(run)?;
    evidence::atomic_write(
        &run.join("qualification.json"),
        &serde_json::to_vec(&state.verification)?,
    )?;
    if notify && state.terminal && !run.join("qualification-notification.json").exists() {
        let event = serde_json::json!({"event": "qualification", "run": run, "details": state.verification});
        evidence::atomic_write(
            &run.join("qualification-notification.json"),
            &serde_json::to_vec(&event)?,
        )?;
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            // Notification delivery is diagnostic, never acceptance evidence.
            if let Err(error) = evidence::bounded_diagnostic(
                vec![
                    "notify-send".into(),
                    "--app-name=HarborDB tests".into(),
                    format!("HarborDB qualification: {}", state.run_id),
                    format!("{:?}", state.verification.verdict),
                ],
                std::time::Duration::from_secs(2),
            ) {
                evidence::atomic_write(
                    &run.join("qualification-notification-error.json"),
                    &serde_json::to_vec(&serde_json::json!({"reason": error.to_string()}))?,
                )?;
            }
        }
    }
    Ok(state)
}
