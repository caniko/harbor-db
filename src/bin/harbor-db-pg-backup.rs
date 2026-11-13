use clap::{Args, Parser, Subcommand};
use harbor_db::storage::{Result, pg_backup};
use std::{path::PathBuf, process::ExitCode, time::Duration};

#[derive(Parser)]
#[command(
    about = "Recovery-ready PostgreSQL backup helpers: gap marker, recovery metadata, health, redundancy, admission"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Receiver start-up, before the slot is (re)created: mark a WAL gap if the slot is missing but history exists.
    MarkGap {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        segment_bytes: u64,
        #[arg(long)]
        psql: PathBuf,
        #[arg(long)]
        conninfo: String,
        #[arg(long)]
        slot: String,
    },
    /// After a verified publish without recovery metadata: clear a gap this base post-dates.
    ClearGap {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        started_ms: u64,
    },
    /// After a verified publish: write base/<id>.meta.json then LAST_SUCCESS=<id>, clearing a gap.
    Finalize {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        started_ms: u64,
        #[arg(long)]
        segment_bytes: u64,
        /// PostgreSQL package providing bin/pg_controldata.
        #[arg(long)]
        package: PathBuf,
        #[arg(long)]
        source_host: String,
        #[arg(long)]
        sql_psql: PathBuf,
        #[arg(long)]
        sql_conninfo: String,
        #[arg(long, default_value_t = 600)]
        wal_wait_seconds: u64,
    },
    /// Fail when the backup chain cannot support recovery.
    BackupHealth(HealthArgs),
    /// systemd ExecCondition: exit 1 (skip) when the chain is healthy and LAST_SUCCESS is
    /// younger than --within-seconds. HARBOR_DB_FORCE_BASE_BACKUP=1 always admits.
    Redundant {
        #[command(flatten)]
        health: HealthArgs,
        #[arg(long)]
        within_seconds: u64,
    },
    /// systemd ExecCondition: exit 1 (skip) when memory is short, 255 (fail) when unreadable.
    Admit {
        #[arg(long, default_value_t = 0)]
        min_ram_bytes: u64,
        #[arg(long, default_value_t = 0)]
        min_swap_bytes: u64,
        #[arg(long, default_value = "/proc/meminfo")]
        meminfo: PathBuf,
    },
}

#[derive(Args)]
struct HealthArgs {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    segment_bytes: u64,
    #[arg(long)]
    slot: String,
    #[arg(long)]
    psql: PathBuf,
    #[arg(long)]
    conninfo: String,
    #[arg(long)]
    systemctl: Option<PathBuf>,
    #[arg(long)]
    receiver_unit: Option<String>,
    #[arg(long, default_value_t = 7200)]
    max_wal_age_seconds: u64,
    #[arg(long)]
    max_base_age_seconds: Option<u64>,
    /// The target publishes recovery metadata: LAST_SUCCESS must name a finalized base.
    #[arg(long)]
    recovery_meta: bool,
}
impl HealthArgs {
    fn health(&self) -> pg_backup::Health<'_> {
        pg_backup::Health {
            root: &self.root,
            segment_bytes: self.segment_bytes,
            slot: &self.slot,
            psql: &self.psql,
            conninfo: &self.conninfo,
            systemctl: self.systemctl.as_deref().zip(self.receiver_unit.as_deref()),
            max_wal_age: self.max_wal_age_seconds,
            max_base_age: self.max_base_age_seconds,
            recovery_meta: self.recovery_meta,
        }
    }
}

fn run() -> Result<ExitCode> {
    // A usage error must fail an ExecCondition (255), not skip it (clap exits 2).
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return Ok(ExitCode::from(if error.use_stderr() { 255 } else { 0 }));
        }
    };
    match cli.command {
        Action::MarkGap {
            root,
            segment_bytes,
            psql,
            conninfo,
            slot,
        } => {
            if pg_backup::mark_gap(&root, segment_bytes, &psql, &conninfo, &slot)? {
                eprintln!("harbor-db-pg-backup: slot {slot} was missing; WAL gap marked");
            }
        }
        Action::ClearGap { root, started_ms } => {
            if pg_backup::clear_gap(&root, started_ms)? {
                eprintln!("harbor-db-pg-backup: WAL gap cleared by the new base backup");
            }
        }
        Action::Finalize {
            root,
            id,
            started_ms,
            segment_bytes,
            package,
            source_host,
            sql_psql,
            sql_conninfo,
            wal_wait_seconds,
        } => {
            pg_backup::finalize(&pg_backup::Finalize {
                root: &root,
                id: &id,
                started: started_ms,
                segment_bytes,
                package: &package,
                source_host: &source_host,
                sql_psql: &sql_psql,
                sql_conninfo: &sql_conninfo,
                wal_wait: Duration::from_secs(wal_wait_seconds),
            })?;
        }
        Action::BackupHealth(args) => {
            let failures = pg_backup::health(&args.health());
            for failure in &failures {
                eprintln!("harbor-db-pg-backup: unhealthy: {failure}");
            }
            if !failures.is_empty() {
                return Ok(ExitCode::from(1));
            }
        }
        Action::Redundant {
            health,
            within_seconds,
        } => {
            if std::env::var_os("HARBOR_DB_FORCE_BASE_BACKUP").is_some_and(|v| !v.is_empty()) {
                eprintln!(
                    "harbor-db-pg-backup: HARBOR_DB_FORCE_BASE_BACKUP set; base backup forced"
                );
            } else if let Some(reason) = pg_backup::redundant(&health.health(), within_seconds) {
                eprintln!("harbor-db-pg-backup: base backup skipped as redundant: {reason}");
                return Ok(ExitCode::from(1));
            }
        }
        Action::Admit {
            min_ram_bytes,
            min_swap_bytes,
            meminfo,
        } => {
            let text = std::fs::read_to_string(&meminfo)?;
            match pg_backup::admit(&text, min_ram_bytes, min_swap_bytes)? {
                pg_backup::Admission::Admitted(message) => {
                    eprintln!("harbor-db-pg-backup: {message}")
                }
                pg_backup::Admission::Refused(message) => {
                    eprintln!("harbor-db-pg-backup: base backup skipped: {message}");
                    return Ok(ExitCode::from(1));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    run().unwrap_or_else(|error| {
        eprintln!("harbor-db-pg-backup: {error}");
        ExitCode::from(255)
    })
}
