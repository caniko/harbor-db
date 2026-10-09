use clap::Parser;
use harbor_db::storage::{Result, backup, invalid};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Conservative retention of complete base backups and their WAL chains")]
struct Args {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    base_days: i64,
    #[arg(long)]
    wal_days: i64,
    #[arg(long)]
    segment_bytes: u64,
}
fn operate(args: Args) -> Result<()> {
    if args.base_days < 1 || args.wal_days <= args.base_days {
        return Err(invalid("require wal-days >= base-days + 1 > 1"));
    }
    backup::prune(
        &args.root,
        args.base_days,
        args.wal_days,
        args.segment_bytes,
        None,
    )
}
fn main() {
    if let Err(error) = operate(Args::parse()) {
        eprintln!("harbor-db-backup-prune: {error}");
        std::process::exit(1);
    }
}
