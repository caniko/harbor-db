use clap::{Parser, Subcommand};
use harbor_db::storage::{
    Result, application_transition as transition, codec, durable, invalid, string,
};
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Explicit resumable backend cutover. Publication never starts or thaws writers")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Plan {
        #[arg(long)]
        candidate: String,
        #[arg(long)]
        writer_fence_token: Option<String>,
    },
    BindCandidate {
        #[arg(long)]
        candidate: String,
    },
    Status,
    Prepare,
    Commit,
    EnableWrites,
    Complete,
    Abort,
    Retire,
}
fn run(cli: Cli) -> Result<()> {
    let path = durable::immutable_config_path(&cli.config).map_err(|error| {
        invalid(format!(
            "operator transition manifests must be immutable store files: {error}"
        ))
    })?;
    let config = durable::read_config_json(&path)?;
    if ["source_manifest", "target_manifest", "backup_manifest"]
        .iter()
        .any(|k| match string(&config, k) {
            Ok(p) => durable::immutable_config_path(std::path::Path::new(p)).is_err(),
            Err(_) => true,
        })
    {
        return Err(invalid(
            "operator transition manifests must be immutable store files",
        ));
    }
    let result = match cli.command {
        Command::Plan {
            candidate,
            writer_fence_token,
        } => transition::plan(&config, &candidate, writer_fence_token.as_deref())?,
        Command::BindCandidate { candidate } => transition::bind_candidate(&config, &candidate)?,
        Command::Status => transition::status(&config)?,
        Command::Prepare => transition::prepare(&config)?,
        Command::Commit => transition::commit(&config)?,
        Command::EnableWrites => transition::enable_writes(&config)?,
        Command::Complete => transition::complete(&config)?,
        Command::Abort => transition::abort(&config)?,
        Command::Retire => transition::retire(&config)?,
    };
    println!(
        "{}",
        String::from_utf8(codec::encode(&result, false)?)
            .map_err(|_| invalid("invalid receipt encoding"))?
    );
    Ok(())
}
fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("harbor-db-transition: {e}");
        std::process::exit(1);
    }
}
