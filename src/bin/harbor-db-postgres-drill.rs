use clap::{Parser, ValueEnum};
use harbor_db::storage::postgres_drill;
use std::path::PathBuf;

#[derive(Clone, ValueEnum)]
enum Operation {
    Restore,
    Cleanup,
}
#[derive(Parser)]
#[command(about = "Restore a logical dump into a private disposable Unix-socket-only cluster")]
struct Args {
    #[arg(long)]
    package: PathBuf,
    #[arg(long, default_value = "database.dump")]
    dump: String,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout_seconds: Option<u64>,
    #[arg(value_enum)]
    command: Operation,
    backup: PathBuf,
    workspace: PathBuf,
}
fn main() {
    let args = Args::parse();
    let command = match args.command {
        Operation::Restore => "restore",
        Operation::Cleanup => "cleanup",
    };
    let result = postgres_drill::timeout_seconds(args.timeout_seconds).and_then(|timeout| {
        postgres_drill::operate_with_timeout(
            &args.package,
            command,
            &args.backup,
            &args.workspace,
            &args.dump,
            timeout,
        )
    });
    if let Err(error) = result {
        eprintln!("harbor-db-postgres-drill: {error}");
        std::process::exit(1);
    }
}
