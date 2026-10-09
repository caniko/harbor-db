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
    if let Err(error) = postgres_drill::operate(
        &args.package,
        command,
        &args.backup,
        &args.workspace,
        &args.dump,
    ) {
        eprintln!("harbor-db-postgres-drill: {error}");
        std::process::exit(1);
    }
}
