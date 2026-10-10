use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use super::error::MigrationError;

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
    /// Reactive repair of an unhealthy resource. Never runs at activation;
    /// executed on demand through `harbor-db restore`.
    Restore,
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
    /// TypeDB (strongly-typed graph database). Readiness, compatibility, and
    /// schema semantics are owned by the application binaries, which report
    /// through the exit-code contract (0 ready/current, 2 pending); harbor-db
    /// provides ordering, credential delivery, and lifecycle modes only.
    #[serde(rename = "typedb")]
    Typedb,
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
    /// The plan contains only names. At runtime harbor-db resolves each name
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
    /// operations but makes them unavailable to `harbor-db check`.
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

/// Neutral name for [`MigrationPlan`] while the harbor-db wire/API name is
/// kept for compatibility. The serialized format remains unchanged.
pub type DatabasePlan = MigrationPlan;

/// Neutral name for [`MigrationOperation`] while the harbor-db wire/API
/// name is kept for compatibility.
pub type DatabaseOperation = MigrationOperation;

/// Generic name for [`MigrationPlan`].
pub type Plan = MigrationPlan;

/// Generic name for [`MigrationOperation`].
pub type Operation = MigrationOperation;

/// Neutral name for [`Backend`] while the harbor-db API remains compatible.
pub type DatabaseBackend = Backend;

/// Neutral name for [`Phase`] while the harbor-db API remains compatible.
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

    pub(super) fn execution_order(
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

/// Apply, check, or restore mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    /// Apply selected operations.
    Apply,
    /// Run read-only checks for selected operations.
    Check,
    /// Repair selected operations: check, apply only what is pending, verify.
    Restore,
}

impl RunMode {
    /// Whether this mode runs apply commands.
    pub fn applies(self) -> bool {
        matches!(self, Self::Apply | Self::Restore)
    }
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
    /// Restore applied a pending operation and the re-check reported current.
    Restored,
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

impl fmt::Display for OperationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Applied => "applied",
            Self::Current => "current",
            Self::Pending => "pending",
            Self::SkippedManual => "skipped-manual",
            Self::Restored => "restored",
        })
    }
}
