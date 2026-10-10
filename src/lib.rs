//! Secure, generic lifecycle-operation plans.
//!
//! DB Harbor owns the lifecycle contract while the project or database owner
//! supplies the actual commands. This keeps deployment orchestration reusable
//! for schema changes, credential provisioning, backfills, backups,
//! maintenance, and operational cutovers without moving domain knowledge into
//! this crate.

mod planner;

pub use planner::*;

pub mod storage;

#[cfg(feature = "testing")]
pub mod testing;

#[cfg(test)]
#[path = "planner/tests.rs"]
mod tests;
