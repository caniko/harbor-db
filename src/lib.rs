//! Secure, generic lifecycle-operation plans.
//!
//! DB Harbor owns the lifecycle contract while the project or database owner
//! supplies the actual commands. This keeps deployment orchestration reusable
//! for schema changes, credential provisioning, backfills, backups,
//! maintenance, and operational cutovers without moving domain knowledge into
//! this crate.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    path::Path,
    process::Stdio,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

/// The current serialized lifecycle-operation plan format.
pub const PLAN_VERSION: u32 = 1;

/// The broad kind of lifecycle operation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// An operation owned by a database or migration backend.
    #[default]
    Database,
    /// A project operation without database-specific semantics.
    Generic,
    /// A project operation that reconciles a credential-backed resource.
    Credential,
}

/// The idempotent lifecycle contract for an operation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Bring the resource to the declared state; repeatable on activation.
    #[default]
    Ensure,
    /// Reconcile an already-created resource with the declared state.
    Reconcile,
}

/// A database family used by an operation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// PostgreSQL, including SQLx and SeaORM-backed schemas.
    Postgres,
    /// ClickHouse, including schema reconciliation and operational changes.
    #[serde(rename = "clickhouse")]
    ClickHouse,
    /// A command that does not need database-specific semantics.
    #[default]
    Generic,
}

/// The lifecycle phase of a database operation.
#[derive(Clone, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Creates or upgrades the normal application schema.
    #[default]
    Schema,
    /// Reconciles or backfills data after the schema exists.
    Backfill,
    /// An explicit operational or potentially destructive change.
    Operational,
}

/// Whether an operation may run during normal deployment activation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Runs when the plan is applied without an explicit operation selector.
    #[default]
    Automatic,
    /// Requires an explicit operation selector and `--confirm` on apply.
    OperatorConfirmed,
}

/// A process invocation represented without a shell command string.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandSpec {
    /// Executable path.
    pub program: String,
    /// Positional arguments passed to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment overrides for this invocation.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Credential names appended as file paths to the invocation arguments.
    ///
    /// The plan contains only names. At runtime db-harbor resolves each name
    /// below systemd's `CREDENTIALS_DIRECTORY`; it never reads or serializes
    /// the credential contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_args: Vec<String>,
    /// Environment variables whose values are credential file paths.
    ///
    /// The map values are credential names, not secret values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_environment: BTreeMap<String, String>,
}

impl CommandSpec {
    /// Construct a command from an executable and argument list.
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            environment: BTreeMap::new(),
            credential_args: Vec::new(),
            credential_environment: BTreeMap::new(),
        }
    }

    fn validate(&self, context: &str) -> Result<(), MigrationError> {
        if self.program.trim().is_empty() {
            return Err(MigrationError::InvalidPlan(format!(
                "{context}: command program must not be empty"
            )));
        }
        for credential in &self.credential_args {
            validate_credential_name(credential, context)?;
        }
        for (environment, credential) in &self.credential_environment {
            if environment.trim().is_empty() {
                return Err(MigrationError::InvalidPlan(format!(
                    "{context}: credential environment name must not be empty"
                )));
            }
            if self.environment.contains_key(environment) {
                return Err(MigrationError::InvalidPlan(format!(
                    "{context}: credential environment {environment} conflicts with environment"
                )));
            }
            validate_credential_name(credential, context)?;
        }
        Ok(())
    }
}

/// One ordered apply/check operation in a database-operation plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationOperation {
    /// Stable operation identifier within its plan.
    pub id: String,
    /// Broad operation kind. Missing in v1 manifests, where it defaults to
    /// `database` without changing execution behavior.
    #[serde(default)]
    pub kind: OperationKind,
    /// Idempotent lifecycle metadata for reporting and policy consumers.
    #[serde(default)]
    pub lifecycle: Lifecycle,
    /// Database family owned by the operation.
    #[serde(default)]
    pub backend: Backend,
    /// Lifecycle phase used for human-readable reporting and policy checks.
    #[serde(default)]
    pub phase: Phase,
    /// Deployment safety policy.
    #[serde(default)]
    pub safety: Safety,
    /// Command that applies the operation idempotently.
    pub apply: CommandSpec,
    /// Optional read-only command. A missing command is allowed for apply-only
    /// operations but makes them unavailable to `db-harbor check`.
    #[serde(default)]
    pub check: Option<CommandSpec>,
    /// Operations that must complete before this operation.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// A versioned set of database operations for one service or deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationPlan {
    /// Serialized plan format version.
    pub version: u32,
    /// Human-readable service or project name.
    pub name: String,
    /// Operations in declaration order. Dependencies determine execution order.
    pub operations: Vec<MigrationOperation>,
}

/// Neutral name for [`MigrationPlan`] while the db-harbor wire/API name is
/// kept for compatibility. The serialized format remains unchanged.
pub type DatabasePlan = MigrationPlan;

/// Neutral name for [`MigrationOperation`] while the db-harbor wire/API
/// name is kept for compatibility.
pub type DatabaseOperation = MigrationOperation;

/// Generic name for [`MigrationPlan`].
pub type Plan = MigrationPlan;

/// Generic name for [`MigrationOperation`].
pub type Operation = MigrationOperation;

/// Neutral name for [`Backend`] while the db-harbor API remains compatible.
pub type DatabaseBackend = Backend;

/// Neutral name for [`Phase`] while the db-harbor API remains compatible.
pub type OperationPhase = Phase;

impl MigrationPlan {
    /// Validate identifiers, references, commands, and dependency cycles.
    pub fn validate(&self) -> Result<(), MigrationError> {
        if self.version != PLAN_VERSION {
            return Err(MigrationError::UnsupportedPlanVersion(self.version));
        }
        if self.name.trim().is_empty() {
            return Err(MigrationError::InvalidPlan(
                "plan name must not be empty".to_owned(),
            ));
        }
        let mut ids = BTreeSet::new();
        for operation in &self.operations {
            if operation.id.trim().is_empty() {
                return Err(MigrationError::InvalidPlan(
                    "operation id must not be empty".to_owned(),
                ));
            }
            if !ids.insert(operation.id.clone()) {
                return Err(MigrationError::DuplicateOperation(operation.id.clone()));
            }
            operation
                .apply
                .validate(&format!("operation {} apply", operation.id))?;
            if let Some(check) = &operation.check {
                check.validate(&format!("operation {} check", operation.id))?;
            }
        }
        for operation in &self.operations {
            for dependency in &operation.depends_on {
                if !ids.contains(dependency) {
                    return Err(MigrationError::MissingDependency {
                        operation: operation.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }
        self.execution_order(&BTreeSet::new())?;
        Ok(())
    }

    fn operation_map(&self) -> BTreeMap<&str, &MigrationOperation> {
        self.operations
            .iter()
            .map(|operation| (operation.id.as_str(), operation))
            .collect()
    }

    fn execution_order(
        &self,
        selected: &BTreeSet<String>,
    ) -> Result<Vec<&MigrationOperation>, MigrationError> {
        let operations = self.operation_map();
        let include_all = selected.is_empty();
        let mut included = BTreeSet::new();

        fn include_dependencies(
            id: &str,
            operations: &BTreeMap<&str, &MigrationOperation>,
            included: &mut BTreeSet<String>,
        ) -> Result<(), MigrationError> {
            if !included.insert(id.to_owned()) {
                return Ok(());
            }
            let operation = operations
                .get(id)
                .ok_or_else(|| MigrationError::MissingOperation(id.to_owned()))?;
            for dependency in &operation.depends_on {
                include_dependencies(dependency, operations, included)?;
            }
            Ok(())
        }

        if include_all {
            for operation in &self.operations {
                include_dependencies(&operation.id, &operations, &mut included)?;
            }
        } else {
            for id in selected {
                if !operations.contains_key(id.as_str()) {
                    return Err(MigrationError::MissingOperation(id.clone()));
                }
                include_dependencies(id, &operations, &mut included)?;
            }
        }

        let mut result = Vec::with_capacity(included.len());
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();

        fn visit<'a>(
            id: &str,
            operations: &BTreeMap<&'a str, &'a MigrationOperation>,
            included: &BTreeSet<String>,
            visiting: &mut BTreeSet<String>,
            visited: &mut BTreeSet<String>,
            result: &mut Vec<&'a MigrationOperation>,
        ) -> Result<(), MigrationError> {
            if visited.contains(id) {
                return Ok(());
            }
            if !visiting.insert(id.to_owned()) {
                return Err(MigrationError::DependencyCycle(id.to_owned()));
            }
            let operation = operations
                .get(id)
                .ok_or_else(|| MigrationError::MissingOperation(id.to_owned()))?;
            for dependency in &operation.depends_on {
                if included.contains(dependency) {
                    visit(dependency, operations, included, visiting, visited, result)?;
                }
            }
            visiting.remove(id);
            visited.insert(id.to_owned());
            result.push(*operation);
            Ok(())
        }

        for operation in &self.operations {
            if included.contains(&operation.id) {
                visit(
                    &operation.id,
                    &operations,
                    &included,
                    &mut visiting,
                    &mut visited,
                    &mut result,
                )?;
            }
        }
        Ok(result)
    }
}

/// Apply or check mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    /// Apply selected operations.
    Apply,
    /// Run read-only checks for selected operations.
    Check,
}

/// Options controlling one plan execution.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Explicit operation selection. Empty means all automatic operations.
    pub operations: BTreeSet<String>,
    /// Permit operator-confirmed operations during apply.
    pub confirm: bool,
}

/// Result for one operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationStatus {
    /// Apply command completed successfully.
    Applied,
    /// Check command reported the database is current.
    Current,
    /// Check command reported pending state using exit code 2.
    Pending,
    /// Operator-only operation was not selected during a normal run.
    SkippedManual,
}

/// Execution report returned by the library and CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunReport {
    /// Plan name.
    pub plan: String,
    /// Per-operation results in execution order.
    pub operations: Vec<(String, OperationStatus)>,
}

impl RunReport {
    /// Whether any check reported pending work.
    pub fn is_pending(&self) -> bool {
        self.operations
            .iter()
            .any(|(_, status)| *status == OperationStatus::Pending)
    }
}

/// Typed migration failures.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// The plan is malformed or internally inconsistent.
    #[error("invalid migration plan: {0}")]
    InvalidPlan(String),
    /// The serialized plan version is newer than this binary supports.
    #[error("unsupported migration plan version {0}; supported version is {PLAN_VERSION}")]
    UnsupportedPlanVersion(u32),
    /// An operation identifier appeared more than once.
    #[error("duplicate migration operation {0}")]
    DuplicateOperation(String),
    /// A dependency names no operation.
    #[error("operation {operation} depends on missing operation {dependency}")]
    MissingDependency {
        operation: String,
        dependency: String,
    },
    /// A selected operation does not exist.
    #[error("selected migration operation does not exist: {0}")]
    MissingOperation(String),
    /// A credential reference is not a safe systemd credential name.
    #[error("invalid credential name {credential} in {context}")]
    InvalidCredentialName { credential: String, context: String },
    /// A credential-backed command was not started by a unit with credentials.
    #[error("operation {operation} requires systemd CREDENTIALS_DIRECTORY")]
    MissingCredentialsDirectory { operation: String },
    /// Dependency graph contains a cycle.
    #[error("migration dependency cycle includes {0}")]
    DependencyCycle(String),
    /// An operator-confirmed operation was selected without confirmation.
    #[error("operation {0} requires explicit confirmation")]
    ConfirmationRequired(String),
    /// A check operation has no read-only command.
    #[error("operation {0} has no check command")]
    MissingCheckCommand(String),
    /// Plan file could not be read.
    #[error("read migration plan {path}: {source}")]
    ReadPlan {
        path: String,
        source: std::io::Error,
    },
    /// Plan file could not be decoded.
    #[error("decode migration plan {path}: {details}")]
    DecodePlan { path: String, details: String },
    /// A migration command could not start.
    #[error("start {operation} command {program}: {source}")]
    StartCommand {
        operation: String,
        program: String,
        source: std::io::Error,
    },
    /// A migration command failed.
    #[error("{operation} command exited with status {status}")]
    CommandFailed { operation: String, status: i32 },
}

fn validate_credential_name(name: &str, context: &str) -> Result<(), MigrationError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(MigrationError::InvalidCredentialName {
            credential: name.to_owned(),
            context: context.to_owned(),
        });
    }
    Ok(())
}

/// Read a JSON or TOML plan based on its file extension.
pub async fn load_plan(path: impl AsRef<Path>) -> Result<MigrationPlan, MigrationError> {
    let path = path.as_ref();
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| MigrationError::ReadPlan {
            path: path.display().to_string(),
            source,
        })?;
    let plan: MigrationPlan = match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => {
            serde_json::from_slice(&bytes).map_err(|error| MigrationError::DecodePlan {
                path: path.display().to_string(),
                details: error.to_string(),
            })?
        }
        _ => toml::from_str(std::str::from_utf8(&bytes).map_err(|error| {
            MigrationError::DecodePlan {
                path: path.display().to_string(),
                details: error.to_string(),
            }
        })?)
        .map_err(|error| MigrationError::DecodePlan {
            path: path.display().to_string(),
            details: error.to_string(),
        })?,
    };
    plan.validate()?;
    Ok(plan)
}

/// Execute a migration plan without invoking a shell.
pub async fn run_plan(
    plan: &MigrationPlan,
    mode: RunMode,
    options: &RunOptions,
) -> Result<RunReport, MigrationError> {
    plan.validate()?;
    let selected = options.operations.clone();
    let order = plan.execution_order(&selected)?;
    let selected_explicitly = !selected.is_empty();
    if mode == RunMode::Apply
        && selected_explicitly
        && !options.confirm
        && let Some(operation) = order.iter().find(|operation| {
            operation.safety == Safety::OperatorConfirmed && selected.contains(&operation.id)
        })
    {
        return Err(MigrationError::ConfirmationRequired(operation.id.clone()));
    }
    let mut report = RunReport {
        plan: plan.name.clone(),
        operations: Vec::new(),
    };

    for operation in order {
        let explicitly_selected = selected.contains(&operation.id);
        if operation.safety == Safety::OperatorConfirmed
            && mode == RunMode::Apply
            && (!selected_explicitly || !explicitly_selected)
        {
            report
                .operations
                .push((operation.id.clone(), OperationStatus::SkippedManual));
            continue;
        }
        let command = match mode {
            RunMode::Apply => &operation.apply,
            RunMode::Check => operation
                .check
                .as_ref()
                .ok_or_else(|| MigrationError::MissingCheckCommand(operation.id.clone()))?,
        };
        let status = run_command(&operation.id, command, mode).await?;
        report.operations.push((operation.id.clone(), status));
    }
    Ok(report)
}

async fn run_command(
    operation: &str,
    spec: &CommandSpec,
    mode: RunMode,
) -> Result<OperationStatus, MigrationError> {
    let credential_directory =
        if spec.credential_args.is_empty() && spec.credential_environment.is_empty() {
            None
        } else {
            Some(
                env::var_os("CREDENTIALS_DIRECTORY")
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| MigrationError::MissingCredentialsDirectory {
                        operation: operation.to_owned(),
                    })?,
            )
        };

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(&spec.environment)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(directory) = credential_directory {
        for credential in &spec.credential_args {
            command.arg(directory.join(credential));
        }
        for (environment, credential) in &spec.credential_environment {
            command.env(environment, directory.join(credential));
        }
    }
    let status = command
        .status()
        .await
        .map_err(|source| MigrationError::StartCommand {
            operation: operation.to_owned(),
            program: spec.program.clone(),
            source,
        })?;
    if status.success() {
        return Ok(match mode {
            RunMode::Apply => OperationStatus::Applied,
            RunMode::Check => OperationStatus::Current,
        });
    }
    if mode == RunMode::Check && status.code() == Some(2) {
        return Ok(OperationStatus::Pending);
    }
    Err(MigrationError::CommandFailed {
        operation: operation.to_owned(),
        status: status.code().unwrap_or(1),
    })
}

impl fmt::Display for OperationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Applied => "applied",
            Self::Current => "current",
            Self::Pending => "pending",
            Self::SkippedManual => "skipped-manual",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(program: &str) -> CommandSpec {
        CommandSpec::new(program, ["-c", "exit 0"])
    }

    fn operation(id: &str, depends_on: &[&str]) -> MigrationOperation {
        MigrationOperation {
            id: id.to_owned(),
            kind: OperationKind::Generic,
            lifecycle: Lifecycle::Ensure,
            backend: Backend::Generic,
            phase: Phase::Schema,
            safety: Safety::Automatic,
            apply: command("sh"),
            check: Some(command("sh")),
            depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn plan(operations: Vec<MigrationOperation>) -> MigrationPlan {
        MigrationPlan {
            version: PLAN_VERSION,
            name: "test".to_owned(),
            operations,
        }
    }

    #[test]
    fn validates_duplicate_and_missing_operations() {
        let duplicate = plan(vec![operation("one", &[]), operation("one", &[])]);
        assert!(matches!(
            duplicate.validate(),
            Err(MigrationError::DuplicateOperation(_))
        ));

        let missing = plan(vec![operation("one", &["missing"])]);
        assert!(matches!(
            missing.validate(),
            Err(MigrationError::MissingDependency { .. })
        ));
    }

    #[test]
    fn detects_dependency_cycles() {
        let cyclic = plan(vec![operation("one", &["two"]), operation("two", &["one"])]);
        assert!(matches!(
            cyclic.validate(),
            Err(MigrationError::DependencyCycle(_))
        ));
    }

    #[test]
    fn structured_command_does_not_split_arguments() {
        let spec = CommandSpec::new("migration", ["--database-url", "postgres:///db?x=a b"]);
        assert_eq!(spec.args, vec!["--database-url", "postgres:///db?x=a b"]);
    }

    #[test]
    fn serializes_clickhouse_backend_name_used_by_nix_plans() {
        assert_eq!(
            serde_json::to_string(&Backend::ClickHouse).expect("backend serializes"),
            "\"clickhouse\""
        );
    }

    #[test]
    fn generic_metadata_and_credential_refs_are_wire_safe() {
        let mut operation = operation("provision", &[]);
        operation.kind = OperationKind::Credential;
        operation.lifecycle = Lifecycle::Ensure;
        operation.apply.credential_args = vec!["password".to_owned()];
        operation
            .apply
            .credential_environment
            .insert("PASSWORD_FILE".to_owned(), "password".to_owned());
        let serialized = serde_json::to_string(&plan(vec![operation])).expect("plan serializes");

        assert!(serialized.contains("\"kind\":\"credential\""));
        assert!(serialized.contains("\"lifecycle\":\"ensure\""));
        assert!(serialized.contains("\"password\""));
        assert!(!serialized.contains("super-secret-value"));
        assert!(!serialized.contains("/run/secrets/password"));
    }

    #[test]
    fn v1_database_shape_still_decodes_without_generic_metadata() {
        let old = r#"{
            "version": 1,
            "name": "legacy",
            "operations": [{
                "id": "schema",
                "backend": "postgres",
                "phase": "schema",
                "apply": {"program": "/bin/true"}
            }]
        }"#;
        let decoded: MigrationPlan = serde_json::from_str(old).expect("legacy plan decodes");
        decoded.validate().expect("legacy plan validates");
        assert_eq!(decoded.operations[0].kind, OperationKind::Database);
        assert_eq!(decoded.operations[0].lifecycle, Lifecycle::Ensure);
    }

    #[test]
    fn credential_references_cannot_escape_the_systemd_directory() {
        let mut operation = operation("provision", &[]);
        operation.apply.credential_args = vec!["../password".to_owned()];

        assert!(matches!(
            plan(vec![operation]).validate(),
            Err(MigrationError::InvalidCredentialName { .. })
        ));
    }

    #[tokio::test]
    async fn apply_runs_dependencies_before_selected_operation() {
        let mut child = operation("child", &["parent"]);
        child.apply = command("sh");
        let report = run_plan(
            &plan(vec![operation("parent", &[]), child]),
            RunMode::Apply,
            &RunOptions {
                operations: ["child".to_owned()].into_iter().collect(),
                confirm: false,
            },
        )
        .await
        .expect("plan runs");
        assert_eq!(
            report.operations,
            vec![
                ("parent".to_owned(), OperationStatus::Applied),
                ("child".to_owned(), OperationStatus::Applied),
            ]
        );
    }

    #[tokio::test]
    async fn operator_operation_requires_selection_and_confirmation() {
        let mut operator = operation("operator", &[]);
        operator.safety = Safety::OperatorConfirmed;
        let plan = plan(vec![operator]);
        let skipped = run_plan(&plan, RunMode::Apply, &RunOptions::default())
            .await
            .expect("manual operation is skipped");
        assert_eq!(skipped.operations[0].1, OperationStatus::SkippedManual);

        let selected = RunOptions {
            operations: ["operator".to_owned()].into_iter().collect(),
            confirm: false,
        };
        assert!(matches!(
            run_plan(&plan, RunMode::Apply, &selected).await,
            Err(MigrationError::ConfirmationRequired(_))
        ));
    }

    #[tokio::test]
    async fn check_preserves_pending_exit_semantics() {
        let mut pending = operation("pending", &[]);
        pending.check = Some(CommandSpec::new("sh", ["-c", "exit 2"]));
        let report = run_plan(&plan(vec![pending]), RunMode::Check, &RunOptions::default())
            .await
            .expect("pending checks are reports, not runner failures");
        assert_eq!(report.operations[0].1, OperationStatus::Pending);
        assert!(report.is_pending());
    }
}
