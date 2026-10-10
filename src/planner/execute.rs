use std::{env, path::Path, process::Stdio};

use tokio::process::Command;

use super::{
    error::MigrationError,
    model::{CommandSpec, MigrationPlan, OperationStatus, RunMode, RunOptions, RunReport, Safety},
};

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
    if mode.applies()
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
            && mode.applies()
            && (!selected_explicitly || !explicitly_selected)
        {
            report
                .operations
                .push((operation.id.clone(), OperationStatus::SkippedManual));
            continue;
        }
        let status = match mode {
            RunMode::Apply => run_command(&operation.id, &operation.apply, RunMode::Apply).await?,
            RunMode::Check => {
                run_command(
                    &operation.id,
                    operation
                        .check
                        .as_ref()
                        .ok_or_else(|| MigrationError::MissingCheckCommand(operation.id.clone()))?,
                    RunMode::Check,
                )
                .await?
            }
            RunMode::Restore => {
                let check = operation
                    .check
                    .as_ref()
                    .ok_or_else(|| MigrationError::MissingCheckCommand(operation.id.clone()))?;
                match run_command(&operation.id, check, RunMode::Check).await? {
                    OperationStatus::Current => OperationStatus::Current,
                    OperationStatus::Pending => {
                        run_command(&operation.id, &operation.apply, RunMode::Apply).await?;
                        match run_command(&operation.id, check, RunMode::Check).await? {
                            OperationStatus::Current => OperationStatus::Restored,
                            status => status,
                        }
                    }
                    status => status,
                }
            }
        };
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
            RunMode::Apply | RunMode::Restore => OperationStatus::Applied,
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
