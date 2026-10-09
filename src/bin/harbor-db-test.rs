use clap::{Args, Parser, Subcommand};
use harbor_db::testing::{
    baseline::Baseline,
    candidate,
    catalog::{Profile, Suite},
    executor, runner,
    supervisor::{self, Verdict},
};
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    name = "harbor-db-test",
    version,
    about = "Candidate-bound lifecycle tests and durable observation"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Args)]
struct Create {
    #[arg(long, conflicts_with = "suite", required_unless_present = "suite")]
    spec: Option<PathBuf>,
    #[arg(long, conflicts_with = "spec")]
    suite: Option<PathBuf>,
    #[arg(long, requires = "suite")]
    source: Option<PathBuf>,
    #[arg(long, default_value = "full", value_parser = ["fast", "integration", "vm", "full"])]
    profile: String,
    #[arg(long)]
    base: PathBuf,
    #[arg(long)]
    id: String,
}
#[derive(Subcommand)]
enum Action {
    CheckBaseline {
        #[arg(long, default_value = "tests/pr14-baseline.toml")]
        baseline: PathBuf,
        #[arg(long, default_value = "tests/suite.toml")]
        suite: PathBuf,
        #[arg(long, default_value = ".")]
        source: PathBuf,
    },
    Catalog {
        #[arg(long)]
        suite: PathBuf,
    },
    Plan {
        #[arg(long)]
        suite: PathBuf,
        #[arg(long, default_value = "full", value_parser = ["fast", "integration", "vm", "full"])]
        profile: String,
    },
    Retain {
        source: PathBuf,
        destination: PathBuf,
    },
    Create(Create),
    Run {
        #[command(flatten)]
        create: Create,
        #[arg(long)]
        foreground: bool,
    },
    Worker {
        directory: PathBuf,
    },
    Observe {
        directory: PathBuf,
        #[arg(long)]
        once: bool,
    },
    Status {
        directory: PathBuf,
    },
    Watch {
        directory: PathBuf,
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
    Cancel {
        directory: PathBuf,
    },
    Verify {
        directory: PathBuf,
    },
    /// Execute one retained, registered harness adapter (normally invoked by a worker).
    Execute {
        #[arg(long)]
        spec: PathBuf,
    },
}
fn profile(value: &str) -> Profile {
    match value {
        "fast" => Profile::Fast,
        "integration" => Profile::Integration,
        "vm" => Profile::Vm,
        _ => Profile::Full,
    }
}
fn print(value: &impl serde::Serialize) -> supervisor::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut output = io::stdout().lock();
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}
fn accepted(verdict: Verdict) -> ExitCode {
    if verdict == Verdict::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
fn create(options: Create) -> supervisor::Result<PathBuf> {
    if let Some(path) = options.spec {
        supervisor::create_run(
            Some(&options.base),
            &options.id,
            serde_json::from_slice(&std::fs::read(path)?)?,
        )
    } else {
        runner::create_with_executor(
            &options.suite.ok_or("suite missing")?,
            &options.source.unwrap_or(std::env::current_dir()?),
            &options.base,
            &options.id,
            profile(&options.profile),
            Some(&std::env::current_exe()?.canonicalize()?),
        )
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("harbor-db-test: {error}");
            ExitCode::from(1)
        }
    }
}
fn run() -> supervisor::Result<ExitCode> {
    match Cli::parse().command {
        Action::CheckBaseline {
            baseline,
            suite,
            source,
        } => {
            let baseline = Baseline::load(baseline)?;
            baseline.validate_migration(&source, &Suite::load(suite)?)?;
            print(
                &serde_json::json!({"schema":1,"status":"baseline_retained","head":baseline.head,
                "python_tests":baseline.python_test_count,"gates":baseline.gates.len(),"runtime_qualified":false}),
            )?;
        }
        Action::Catalog { suite } => print(&Suite::load(suite)?)?,
        Action::Plan {
            suite,
            profile: value,
        } => {
            let suite = Suite::load(suite)?;
            print(
                &serde_json::json!({"cases": suite.select(profile(&value)), "coverage": suite.coverage(profile(&value)), "executed": false}),
            )?;
        }
        Action::Retain {
            source,
            destination,
        } => print(&candidate::retain(&source, &destination)?)?,
        Action::Create(options) => print(&serde_json::json!({"directory": create(options)?}))?,
        Action::Run {
            create: options,
            foreground,
        } => {
            let directory = create(options)?;
            print(&serde_json::json!({"directory": directory}))?;
            if foreground {
                supervisor::foreground(&directory)?;
                let status = runner::publish_verification(&directory, true)?;
                print(&status)?;
                return Ok(accepted(status.verification.verdict));
            }
            let units =
                supervisor::start_detached(&directory, &std::env::current_exe()?.canonicalize()?)?;
            print(&serde_json::json!({"directory": directory, "units": units}))?;
        }
        Action::Worker { directory } => {
            supervisor::worker(&directory)?;
            let status = runner::publish_verification(&directory, false)?;
            print(&status)?;
            return Ok(accepted(status.verification.verdict));
        }
        Action::Observe { directory, once } => {
            if once {
                supervisor::observe_once(&directory)?;
            } else {
                supervisor::observe(&directory)?;
            }
            print(&runner::publish_verification(&directory, true)?)?;
        }
        Action::Status { directory } => print(&runner::status(&directory)?)?,
        Action::Watch {
            directory,
            seconds,
            interval_ms,
        } => {
            let began = Instant::now();
            loop {
                let status = runner::status(&directory)?;
                if let Err(error) = print(&status) {
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
                    {
                        break;
                    }
                    return Err(error);
                }
                if status.terminal
                    || seconds.is_some_and(|s| began.elapsed() >= Duration::from_secs(s))
                    || status.started.is_some() && !status.worker_alive
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(interval_ms.max(10)));
            }
        }
        Action::Cancel { directory } => {
            supervisor::cancel(&directory)?;
            print(&serde_json::json!({"directory": directory, "cancel_requested": true}))?;
        }
        Action::Verify { directory } => {
            let verification = runner::verify(&directory)?;
            print(&verification)?;
            return Ok(accepted(verification.verdict));
        }
        Action::Execute { spec } => executor::execute(&executor::load(&spec)?)?,
    }
    Ok(ExitCode::SUCCESS)
}
