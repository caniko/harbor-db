//! Storage primitives shared by lifecycle engines and qualification tooling.
pub mod application_backup;
pub mod application_transition;
pub mod backup;
pub mod codec;
pub mod custody;
pub mod cutover;
pub mod durable;
pub mod login_shell;
pub mod pg_core;
pub mod postgres;
pub mod postgres_drill;
pub mod process;
pub mod provision;
pub mod recovery;
// The opt-in physical producer and readers preserve legacy recovery contracts.
pub mod recovery_capture;
pub mod recovery_repository;
pub mod resource;
mod retention_json;
pub mod startup_inhibition;
pub mod transition_manifest;
pub mod writer_fence;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::Invalid(message.into())
}

pub fn string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))
}
