use clap::{Parser, Subcommand};
use harbor_db::storage::{Result, durable, resource};
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Check,
    Adopt {
        #[arg(long)]
        identity: String,
    },
    Serve {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}
fn run(args: Args) -> Result<()> {
    let c = durable::read_config_json(&args.config)?;
    match args.command {
        Action::Check => resource::check(&c),
        Action::Adopt { identity } => resource::adopt(&c, &identity).map(|_| ()),
        Action::Serve { mut argv } => {
            if argv.first().is_some_and(|s| s == "--") {
                argv.remove(0);
            }
            resource::serve(&c, &argv)
        }
    }
}
fn main() {
    if let Err(e) = run(Args::parse()) {
        eprintln!("harbor-db-resource: {e}");
        std::process::exit(1);
    }
}
