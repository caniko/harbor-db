use clap::Parser;
use harbor_db::storage::transition_manifest;
#[derive(Parser)]
#[command(about = "Root-owned generation policy gates legacy units lacking a wrapper")]
struct Cli {
    #[arg(long)]
    state: std::path::PathBuf,
    #[arg(long)]
    unit: String,
}
fn main() {
    let cli = Cli::parse();
    if let Err(e) = transition_manifest::startup_unit(&cli.state, &cli.unit) {
        eprintln!("harbor-db-transition-start: {e}");
        std::process::exit(1);
    }
}
