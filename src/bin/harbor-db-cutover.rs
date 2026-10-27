use clap::{Args, Parser, Subcommand};
use harbor_db::storage::{Result, cutover, durable, invalid, string};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    about = "Mandatory read-only cutover admission; explicit filesystem custody certification"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Args)]
struct Contract {
    #[arg(long)]
    contract: PathBuf,
    #[arg(long)]
    host: String,
}
#[derive(Subcommand)]
enum Command {
    Check {
        #[command(flatten)]
        contract: Contract,
        #[arg(long)]
        candidate: Option<String>,
        #[arg(long,default_value="preflight",value_parser=["preflight","activate","startup","certify"])]
        phase: String,
        #[arg(long, hide = true)]
        worker: Option<String>,
        #[arg(long, hide = true, requires = "worker")]
        authority_contract: Option<String>,
        #[arg(
            long,
            hide = true,
            requires = "worker",
            conflicts_with = "authority_contract"
        )]
        authority_contract_stdin: bool,
    },
    Certify {
        #[command(flatten)]
        contract: Contract,
        #[arg(long)]
        resource: String,
        #[arg(long, required = true)]
        restore_root: Vec<PathBuf>,
        #[arg(long)]
        identity: String,
    },
    #[command(hide = true)]
    CertifyWorker {
        #[command(flatten)]
        contract: Contract,
        #[arg(long,value_parser=["certify"])]
        phase: String,
        #[arg(long)]
        worker: String,
        #[arg(long,num_args=1..,required=true)]
        certify_roots: Vec<PathBuf>,
        #[arg(long)]
        certify_identity: String,
        #[arg(long)]
        database_snapshot: Option<String>,
        #[arg(long, default_value = "[]")]
        database_requirements: String,
        #[arg(long, hide = true)]
        namespace_contracts: Option<String>,
        #[arg(long, hide = true, conflicts_with = "namespace_contracts")]
        namespace_contracts_stdin: bool,
    },
    Serve {
        #[command(flatten)]
        contract: Contract,
        #[arg(long)]
        resource: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}
fn run(cli: Cli) -> Result<i32> {
    let contract = match &cli.command {
        Command::Check { contract, .. }
        | Command::Certify { contract, .. }
        | Command::CertifyWorker { contract, .. }
        | Command::Serve { contract, .. } => contract,
    };
    // The legacy cutover contract explicitly supports deployed aliases. Bind
    // workers to the resolved contract file; custody and authority are still
    // read through their strict receipt readers.
    let path = durable::configuration_path(&contract.contract)?;
    let manifest = cutover::validate_manifest(&durable::read_config_json(&path)?, &contract.host)?;
    let select = |name: &str| {
        manifest["resources"]
            .get(name)
            .ok_or_else(|| invalid(format!("unknown cutover resource: {name}")))
    };
    match cli.command {
        Command::Check {
            phase,
            worker,
            candidate,
            authority_contract,
            authority_contract_stdin,
            ..
        } => {
            let result = if let Some(worker) = worker {
                let seconds = manifest[if phase == "preflight" {
                    "timeout_seconds"
                } else {
                    "activation_timeout_seconds"
                }]
                .as_u64()
                .unwrap_or(if phase == "preflight" { 30 } else { 900 });
                let deadline = Instant::now() + Duration::from_secs(seconds);
                let mut pinned = authority_contract
                    .as_deref()
                    .map(harbor_db::storage::codec::decode_str)
                    .transpose()?;
                if authority_contract_stdin {
                    pinned = Some(cutover::read_snapshot(deadline)?);
                }
                cutover::check_resource_until(
                    select(&worker)?,
                    &phase,
                    None,
                    candidate.as_deref(),
                    pinned.as_ref(),
                    Some(deadline),
                )?
            } else {
                cutover::check_manifest(&path, &manifest, &phase, candidate.as_deref())?
            };
            println!("{}", serde_json::to_string(&result)?);
            Ok(i32::from(result["status"] == "blocked"))
        }
        Command::Certify {
            resource,
            restore_root,
            identity,
            ..
        } => {
            let config = select(&resource)?;
            let timeout = Duration::from_secs(
                manifest["activation_timeout_seconds"]
                    .as_u64()
                    .unwrap_or(900),
            );
            let deadline = Instant::now() + timeout;
            let contracts = cutover::validate_restore_namespaces_with_contracts(
                &manifest,
                &restore_root,
                None,
                deadline,
            )?;
            let remaining = || {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or_else(|| invalid("cutover inspection budget exhausted"))
            };
            let mut extra = vec!["--namespace-contracts-stdin".into()];
            if let Some(dependency) = config["database_resource"].as_str() {
                let result = cutover::execute_worker(
                    &path,
                    dependency,
                    select(dependency)?,
                    cutover::WorkerOptions {
                        phase: "certify",
                        timeout: remaining()?,
                        extra: &[],
                        command_name: "check",
                        as_root: false,
                        input: None,
                    },
                )?;
                let hash = string(&result, "database_snapshot_sha256")?;
                if !hash.is_empty() {
                    extra.extend([
                        "--database-snapshot".into(),
                        hash.into(),
                        "--database-requirements".into(),
                        serde_json::to_string(
                            &result["corpus_requirements"]
                                .get(&resource)
                                .cloned()
                                .unwrap_or_else(|| json!([])),
                        )?,
                    ]);
                }
            }
            extra.extend([
                "--certify-identity".into(),
                identity,
                "--certify-roots".into(),
            ]);
            extra.extend(
                restore_root
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
            cutover::execute_worker(
                &path,
                &resource,
                config,
                cutover::WorkerOptions {
                    phase: "certify",
                    timeout: remaining()?,
                    extra: &extra,
                    command_name: "certify-worker",
                    // Inspect private global namespaces in the certifying child
                    // before it permanently drops to the selected resource user.
                    as_root: unsafe { libc::geteuid() } == 0,
                    input: Some(&contracts),
                },
            )?;
            Ok(0)
        }
        Command::CertifyWorker {
            worker,
            certify_roots,
            certify_identity,
            database_snapshot,
            database_requirements,
            namespace_contracts,
            namespace_contracts_stdin,
            ..
        } => {
            let mut pinned = namespace_contracts
                .as_deref()
                .map(harbor_db::storage::codec::decode_str)
                .transpose()?;
            let deadline = Instant::now()
                + Duration::from_secs(
                    manifest["activation_timeout_seconds"]
                        .as_u64()
                        .unwrap_or(900),
                );
            if namespace_contracts_stdin {
                pinned = Some(cutover::read_snapshot(deadline)?);
            }
            let contracts = cutover::validate_restore_namespaces_with_contracts(
                &manifest,
                &certify_roots,
                pinned.as_ref(),
                deadline,
            )?;
            cutover::enter_certification_user(select(&worker)?)?;
            let requirements = harbor_db::storage::codec::decode_str(&database_requirements)?;
            cutover::certify_filesystem_with_context(
                select(&worker)?,
                &certify_roots,
                &certify_identity,
                None,
                database_snapshot.as_deref(),
                &requirements,
                cutover::CertificationContext {
                    pinned: contracts.get(&worker),
                    deadline: Some(deadline),
                },
            )?;
            println!("{{}}");
            Ok(0)
        }
        Command::Serve {
            resource, mut argv, ..
        } => {
            if argv.first().is_some_and(|s| s == "--") {
                argv.remove(0);
            }
            cutover::serve(select(&resource)?, &argv)?;
            Ok(0)
        }
    }
}
fn main() {
    let code = match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("harbor-db-cutover: {e}");
            1
        }
    };
    std::process::exit(code);
}
