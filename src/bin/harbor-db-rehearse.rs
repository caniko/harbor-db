use clap::Parser;
use harbor_db::storage::rehearsal;
use serde_json::json;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    about = "Rehearse migrations and maintenance on a restored copy of a database, then write a receipt"
)]
struct Args {
    /// Plan JSON (schema harbor-db.rehearsal-plan.v1)
    #[arg(long)]
    plan: PathBuf,
    /// Directory receiving <utc-timestamp>.json
    #[arg(long)]
    receipt_dir: PathBuf,
}

fn main() {
    let args = Args::parse();
    let outcome = rehearsal::read_plan(&args.plan)
        .and_then(|plan| rehearsal::rehearse(&plan, &args.receipt_dir));
    match outcome {
        Ok(outcome) => {
            println!(
                "{}",
                json!({
                    "receipt": outcome.receipt,
                    "receipt_sha256": outcome.receipt_sha256,
                    "status": if outcome.passed { "passed" } else { "failed" },
                })
            );
            if !outcome.passed {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("harbor-db-rehearse: {error}");
            std::process::exit(1);
        }
    }
}
