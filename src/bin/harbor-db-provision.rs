use clap::{Parser, ValueEnum};
use harbor_db::storage::{Result, durable, invalid, provision};
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    command: Action,
}
#[derive(Clone, ValueEnum)]
enum Action {
    Apply,
    Check,
}
fn run(a: Args) -> Result<bool> {
    let c = durable::read_config_json(&a.config)?;
    if c.as_object().is_none_or(|m| {
        m.len() != 3
            || !["version", "policy", "endpoint"]
                .iter()
                .all(|k| m.contains_key(*k))
    }) || c["version"] != 1
    {
        return Err(invalid("unsupported application provisioning manifest"));
    }
    match a.command {
        Action::Apply => {
            provision::apply(&c["policy"], &c["endpoint"])?;
            Ok(true)
        }
        Action::Check => provision::check(&c["policy"], &c["endpoint"]),
    }
}
fn main() {
    match run(Args::parse()) {
        Ok(true) => (),
        Ok(false) => std::process::exit(2),
        Err(e) => {
            eprintln!("harbor-db-provision: {e}");
            std::process::exit(1);
        }
    }
}
