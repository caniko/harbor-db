use clap::{Parser, Subcommand, ValueEnum};
use harbor_db::storage::{self, Result, codec, durable, invalid, pg_core, postgres};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Explicit adoption, fail-closed startup and staged PostgreSQL upgrades")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Operation,
}
#[derive(Clone, ValueEnum)]
enum Phase {
    Prepared,
    Closed,
}
impl Phase {
    fn text(&self) -> &str {
        match self {
            Self::Prepared => "prepared",
            Self::Closed => "closed",
        }
    }
}
#[derive(Subcommand)]
enum Operation {
    Check,
    Serve,
    Adopt {
        #[arg(long)]
        system_identifier: String,
    },
    InspectLive {
        #[arg(long)]
        system_identifier: String,
        #[arg(long, default_value = "/run/postgresql")]
        socket_dir: PathBuf,
        #[arg(long, default_value_t = 5432)]
        port: u16,
    },
    AdoptLive {
        #[arg(long)]
        system_identifier: String,
        #[arg(long, default_value = "/run/postgresql")]
        socket_dir: PathBuf,
        #[arg(long, default_value_t = 5432)]
        port: u16,
    },
    InspectRecovery {
        #[arg(long)]
        socket_dir: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
    },
    FenceOpen {
        #[arg(long)]
        system_identifier: String,
    },
    FenceClose {
        #[arg(long)]
        token: String,
    },
    InspectOfflineFence {
        #[arg(long)]
        token: String,
        #[arg(long, value_enum)]
        phase: Phase,
    },
    InhibitStartup {
        #[arg(long)]
        system_identifier: String,
    },
    ReleaseStartup {
        #[arg(long)]
        token: String,
        #[arg(long)]
        fence_token: String,
        #[arg(long, value_enum)]
        phase: Phase,
    },
    InspectFence {
        #[arg(long)]
        token: String,
        #[arg(long, default_value = "/run/postgresql")]
        socket_dir: PathBuf,
        #[arg(long, default_value_t = 5432)]
        port: u16,
    },
    PrepareRecovery {
        #[arg(long)]
        preparation_config: PathBuf,
        #[arg(long)]
        socket_dir: PathBuf,
        #[arg(long)]
        port: u16,
    },
    SnapshotRecords {
        #[arg(long)]
        socket_dir: PathBuf,
        #[arg(long)]
        port: u16,
    },
    CertifyRecovery {
        #[arg(long)]
        socket_dir: PathBuf,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        data_dir: PathBuf,
    },
    Upgrade {
        #[arg(long)]
        retry_incomplete: bool,
    },
}
fn operate(args: Args) -> Result<Option<Value>> {
    let config = durable::read_config_json(&args.config)?;
    let result = match args.command {
        Operation::Adopt { system_identifier } => {
            postgres::adopt(&config, &system_identifier)?;
            return Ok(None);
        }
        Operation::InspectLive {
            system_identifier,
            socket_dir,
            port,
        } => pg_core::inspect_live(&config, &system_identifier, &socket_dir, port)?,
        Operation::AdoptLive {
            system_identifier,
            socket_dir,
            port,
        } => postgres::adopt_live(&config, &system_identifier, &socket_dir, port)?,
        Operation::Check => {
            postgres::check(&config)?;
            return Ok(None);
        }
        Operation::Serve => {
            postgres::serve(&config)?;
            return Ok(None);
        }
        Operation::Upgrade { retry_incomplete } => {
            postgres::upgrade(&config, retry_incomplete)?;
            return Ok(None);
        }
        Operation::FenceOpen { system_identifier } => {
            storage::writer_fence::open_fence(&config, &system_identifier)?
        }
        Operation::FenceClose { token } => storage::writer_fence::close_fence(&config, &token)?,
        Operation::InspectOfflineFence { token, phase } => {
            storage::writer_fence::inspect_offline(&config, &token, phase.text())?
        }
        Operation::InspectFence {
            token,
            socket_dir,
            port,
        } => storage::writer_fence::inspect_live(&config, &token, &socket_dir, port)?,
        Operation::InhibitStartup { system_identifier } => {
            let path = durable::immutable_config_path(&args.config)?;
            storage::startup_inhibition::inhibit(
                &durable::read_config_json(&path)?,
                &system_identifier,
            )?
        }
        Operation::ReleaseStartup {
            token,
            fence_token,
            phase,
        } => {
            let path = durable::immutable_config_path(&args.config)?;
            storage::startup_inhibition::release(
                &durable::read_config_json(&path)?,
                &path,
                &token,
                &fence_token,
                phase.text(),
            )?
        }
        Operation::InspectRecovery { socket_dir, port } => match (socket_dir, port) {
            (None, None) => storage::recovery::check(&config, None)?,
            (Some(socket), Some(port)) => {
                storage::recovery::live_check(&config, &socket, port, None)?
            }
            _ => {
                return Err(invalid(
                    "live recovery inspection requires both local socket and port",
                ));
            }
        },
        Operation::PrepareRecovery {
            preparation_config,
            socket_dir,
            port,
        } => storage::recovery::prepare(
            &config,
            &durable::read_config_json(&preparation_config)?,
            &socket_dir,
            port,
        )?,
        Operation::SnapshotRecords { socket_dir, port } => {
            storage::recovery::snapshot(&config, &socket_dir, port, None)?
        }
        Operation::CertifyRecovery {
            data_dir,
            socket_dir,
            port,
        } => storage::recovery::certify(&config, &data_dir, &socket_dir, port, None, None)?,
    };
    Ok(Some(result))
}
fn main() {
    match operate(Args::parse()) {
        Ok(Some(value)) => match codec::encode(&value, false) {
            Ok(bytes) => println!("{}", String::from_utf8_lossy(&bytes)),
            Err(error) => {
                eprintln!("harbor-db-postgres: {error}");
                std::process::exit(1);
            }
        },
        Ok(None) => {}
        Err(error) => {
            eprintln!("harbor-db-postgres: {error}");
            std::process::exit(1);
        }
    }
}
