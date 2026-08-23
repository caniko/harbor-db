use std::{collections::BTreeSet, path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use harbor_db::{MigrationError, RunMode, RunOptions, load_plan, run_plan};

#[derive(Debug, Parser)]
#[command(
    name = "harbor-db",
    version,
    about = "Apply and check structured lifecycle-operation plans"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply automatic operations, or explicitly selected operator operations.
    Apply(RunArgs),
    /// Run read-only lifecycle-state checks.
    Check(RunArgs),
    /// Repair pending operations: check, apply what is pending, and verify.
    Restore(RunArgs),
    /// Validate a plan without contacting any database.
    Validate(PlanArgs),
}

#[derive(Debug, Args)]
struct PlanArgs {
    /// JSON or TOML lifecycle-operation plan.
    #[arg(long)]
    manifest: PathBuf,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    plan: PlanArgs,
    /// Explicit operation identifier. Repeat for multiple operations.
    #[arg(long = "operation")]
    operations: Vec<String>,
    /// Confirm operator-only apply operations.
    #[arg(long)]
    confirm: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("harbor-db: {error}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode, MigrationError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate(args) => {
            load_plan(args.manifest).await?;
            println!("database-operation plan is valid");
            Ok(ExitCode::SUCCESS)
        }
        Command::Apply(args) => run_plan_command(args, RunMode::Apply).await,
        Command::Check(args) => run_plan_command(args, RunMode::Check).await,
        Command::Restore(args) => run_plan_command(args, RunMode::Restore).await,
    }
}

async fn run_plan_command(args: RunArgs, mode: RunMode) -> Result<ExitCode, MigrationError> {
    let plan = load_plan(&args.plan.manifest).await?;
    let report = run_plan(
        &plan,
        mode,
        &RunOptions {
            operations: args.operations.into_iter().collect::<BTreeSet<_>>(),
            confirm: args.confirm,
        },
    )
    .await?;
    for (operation, status) in &report.operations {
        println!("{operation}: {status}");
    }
    if report.is_pending() {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
