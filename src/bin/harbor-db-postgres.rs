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
    /// Routine admission: backup health without record-certified evidence.
    InspectBackup,
    /// Rebuild libc-collation indexes of databases whose recorded collation
    /// version drifted, then refresh the version. Emits JSON lines.
    CollationRefresh {
        #[arg(long, default_value = "/run/postgresql")]
        socket_dir: PathBuf,
        #[arg(long, default_value_t = 5432)]
        port: u16,
        /// Print the plan only; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// With --dry-run, exit nonzero when any database drifted.
        #[arg(long, requires = "dry_run")]
        fail_on_drift: bool,
        /// Run bt_index_check where amcheck is already installed.
        #[arg(long)]
        verify: bool,
        /// Restrict to these databases. Repeat for several.
        #[arg(long = "database")]
        databases: Vec<String>,
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
    /// Root-coordinated trusted copy for systemd's off-host receipt credential.
    StageOffHostReceipt {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    SnapshotRecords {
        #[arg(long)]
        socket_dir: PathBuf,
        #[arg(long)]
        port: u16,
    },
    CaptureBackup {
        #[arg(long)]
        backup_id: String,
        #[arg(long)]
        capture_id: String,
        #[arg(long, default_value = "/run/postgresql")]
        socket_dir: PathBuf,
        #[arg(long, default_value_t = 5432)]
        port: u16,
        #[arg(long, default_value_t = 30)]
        wal_wait_seconds: u64,
    },
    CertifyRecovery {
        #[arg(long)]
        socket_dir: PathBuf,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        data_dir: PathBuf,
    },
    /// Retire one obsolete source-local capture, preserving selected/in-use evidence.
    RetireCapture {
        #[arg(long)]
        capture_id: String,
    },
    Upgrade {
        #[arg(long)]
        retry_incomplete: bool,
    },
    /// Coordinate a guarded first rollout from one rollout manifest (`--config`).
    Rollout {
        #[command(subcommand)]
        step: RolloutStep,
    },
    /// Pre-switch guard of a generation without the rollout coordinator: reads
    /// only the fixed rollout journal (`--config` is not consulted).
    #[command(hide = true)]
    RolloutGuard,
}
#[derive(Subcommand)]
enum RolloutStep {
    /// Show the journal phase and the next action (needs read access to the
    /// root-owned journal; takes no lease).
    Status,
    /// Pre-switch guard: refuse activation while a rollout sits between fence
    /// and adoption (installed by the module in place of legacy preparation).
    #[command(hide = true)]
    ActivationCheck,
    /// Perform the next local step, or print the external action.
    Next {
        /// As for `fence`.
        #[arg(long)]
        adopt_fence_token: Option<String>,
    },
    /// Pause publishers, stop clients and open the writer fence.
    Fence {
        /// After an opening interrupted before its token was recorded, adopt
        /// Harbor's live fence by its token (once no other coordinator ran).
        #[arg(long)]
        adopt_fence_token: Option<String>,
    },
    /// Run recovery preparation; stops at the independent-certification hand-off.
    Prepare,
    /// On the executor: restore and certify a transported export.
    Certify {
        #[arg(long)]
        export: PathBuf,
        #[arg(long)]
        export_sha256: String,
        #[arg(long)]
        workspace: PathBuf,
    },
    /// Stage the independent receipt and admit it.
    Import {
        #[arg(long)]
        receipt: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// Re-verify live records and adopt the primary's authority.
    Adopt,
    /// Record that the candidate generation is active under the fence.
    Activated,
    /// After verifying the deployment, write the acceptance `thaw` checks
    /// (root 0600 in the journal root) and print its path in `next`.
    Accept,
    /// Release the fence with a deployment acceptance and restart clients.
    Thaw {
        #[arg(long)]
        acceptance: PathBuf,
    },
    /// Archive a thawed or aborted rollout's journal and drop its GC root,
    /// without starting a new rollout.
    Archive,
    /// Back out before activation, or after rolling back a failed deployment:
    /// close the fence and restart clients.
    Abort {
        /// Explicitly adopt Harbor's live writer fence by its token: with no
        /// journal, or after an opening interrupted before its token was recorded.
        #[arg(long)]
        adopt_fence_token: Option<String>,
        /// With no journal and no live fence, explicitly adopt Harbor's held
        /// startup inhibition by its token.
        #[arg(long, conflicts_with = "adopt_fence_token")]
        adopt_startup_token: Option<String>,
    },
}
fn operate(args: Args) -> Result<Option<Value>> {
    if let Operation::RolloutGuard = args.command {
        return Ok(Some(storage::postgres_rollout::activation_guard()?));
    }
    if let Operation::Rollout { step } = args.command {
        use storage::postgres_rollout::Action;
        let action = match step {
            RolloutStep::Status => Action::Status,
            RolloutStep::ActivationCheck => Action::ActivationCheck,
            RolloutStep::Next { adopt_fence_token } => Action::Next { adopt_fence_token },
            RolloutStep::Fence { adopt_fence_token } => Action::Fence { adopt_fence_token },
            RolloutStep::Prepare => Action::Prepare,
            RolloutStep::Certify {
                export,
                export_sha256,
                workspace,
            } => Action::Certify {
                export,
                export_sha256,
                workspace,
            },
            RolloutStep::Import { receipt, sha256 } => Action::Import { receipt, sha256 },
            RolloutStep::Adopt => Action::Adopt,
            RolloutStep::Activated => Action::Activated,
            RolloutStep::Accept => Action::Accept,
            RolloutStep::Thaw { acceptance } => Action::Thaw { acceptance },
            RolloutStep::Archive => Action::Archive,
            RolloutStep::Abort {
                adopt_fence_token,
                adopt_startup_token,
            } => Action::Abort {
                adopt_fence_token,
                adopt_startup_token,
            },
        };
        return Ok(Some(storage::postgres_rollout::operate(
            &args.config,
            action,
        )?));
    }
    let config = durable::read_config_json(&args.config)?;
    let result = match args.command {
        Operation::StageOffHostReceipt {
            source,
            destination,
        } => {
            storage::recovery::stage_off_host_import(&source, &destination)?;
            return Ok(None);
        }
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
        Operation::CollationRefresh {
            socket_dir,
            port,
            dry_run,
            fail_on_drift,
            verify,
            databases,
        } => {
            storage::collation::run(
                &config,
                &storage::collation::Options {
                    socket_dir,
                    port,
                    dry_run,
                    fail_on_drift,
                    verify,
                    databases,
                },
            )?;
            return Ok(None);
        }
        Operation::Check => {
            postgres::check(&config)?;
            return Ok(None);
        }
        Operation::Serve => {
            postgres::serve(&config)?;
            return Ok(None);
        }
        Operation::Upgrade { retry_incomplete } => {
            postgres::validate_operator_upgrade_policy(&config)?;
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
        Operation::InspectBackup => storage::recovery::check(&config, None)?,
        Operation::InspectRecovery { socket_dir, port } => match (socket_dir, port) {
            (None, None) => storage::recovery::evidence_check(&config, None)?,
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
        Operation::CaptureBackup {
            backup_id,
            capture_id,
            socket_dir,
            port,
            wal_wait_seconds,
        } => storage::recovery_capture::finalize(
            &config,
            &backup_id,
            &capture_id,
            &socket_dir,
            port,
            std::time::Duration::from_secs(wal_wait_seconds),
        )?,
        Operation::RetireCapture { capture_id } => {
            storage::recovery_repository::retire(&config, &capture_id)?
        }
        Operation::CertifyRecovery {
            data_dir,
            socket_dir,
            port,
        } => storage::recovery::certify(&config, &data_dir, &socket_dir, port, None, None)?,
        Operation::Rollout { .. } | Operation::RolloutGuard => {
            unreachable!("handled before policy loading")
        }
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
