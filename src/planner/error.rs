use thiserror::Error;

use super::model::PLAN_VERSION;

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
