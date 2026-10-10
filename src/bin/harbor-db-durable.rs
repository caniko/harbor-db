use clap::{Parser, Subcommand};
use harbor_db::storage::{Result, durable};
use std::{io::Read, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(about = "Durably publish files and offline storage trees")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Write {
        destination: PathBuf,
    },
    PublishTree {
        source: PathBuf,
        destination: PathBuf,
    },
    PublishFile {
        source: PathBuf,
        destination: PathBuf,
    },
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Action::Write { destination } => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes)?;
            durable::atomic_write(&destination, &bytes)
        }
        Action::PublishTree {
            source,
            destination,
        } => durable::publish_tree(&source, &destination),
        Action::PublishFile {
            source,
            destination,
        } => durable::publish_file(&source, &destination),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harbor-db-durable: {error}");
            ExitCode::from(1)
        }
    }
}
