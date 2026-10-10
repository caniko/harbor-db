//! Host-owned orchestration of the PostgreSQL writer-fence NixOS VM gate.
//! The Python bridge supplies transport only; fault injection remains a legacy oracle.
use clap::Parser;
use harbor_db::testing::protocol::{Client, Request};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LIFECYCLE_SECONDS: u64 = 3600;
const COMMAND_SECONDS: u64 = 900;
const WAIT_SECONDS: u64 = 900;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_fd: i32,
    #[arg(long)]
    config: String,
    #[arg(long)]
    legacy_python_path: String,
    #[arg(long)]
    fault_script: String,
    #[arg(long)]
    data_dir: String,
    #[arg(long)]
    startup_dir: String,
    #[arg(long)]
    acceptance: PathBuf,
}

fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message.into()).into())
    }
}

struct Gate {
    client: Client,
    began: Instant,
    assertions: Vec<String>,
}

impl Gate {
    fn call(&mut self, request: Request) -> Result<Value> {
        require(
            self.began.elapsed() < Duration::from_secs(LIFECYCLE_SECONDS),
            "writer-fence lifecycle budget exhausted",
        )?;
        Ok(self.client.call(request)?)
    }

    fn execute(&mut self, argv: Vec<String>, seconds: u64) -> Result<(i64, String)> {
        let value = self.call(Request::Execute {
            node: "machine".into(),
            argv,
            timeout_seconds: seconds,
        })?;
        let code = value["exit_code"].as_i64().ok_or("missing exit_code")?;
        let output = value["output"].as_str().ok_or("missing output")?.to_owned();
        Ok((code, output))
    }

    fn command(&mut self, argv: Vec<String>, success: bool, name: &str) -> Result<String> {
        let description = format!("{argv:?}");
        let (code, output) = self.execute(argv, COMMAND_SECONDS)?;
        require(
            (code == 0) == success,
            format!("{name}: {description}: exit {code}: {output}"),
        )?;
        self.assertions.push(name.into());
        Ok(output)
    }

    fn shell(&mut self, script: impl Into<String>, success: bool, name: &str) -> Result<String> {
        self.command(vec!["sh".into(), "-c".into(), script.into()], success, name)
    }

    fn unit(&mut self, unit: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECONDS);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            require(
                !remaining.is_zero(),
                format!("waiting for {unit} timed out"),
            )?;
            let (code, output) = self.execute(
                vec!["systemctl".into(), "is-active".into(), unit.into()],
                remaining.as_secs().clamp(1, 60),
            )?;
            if code == 0 && output.trim() == "active" {
                self.assertions.push(format!("unit-active:{unit}"));
                return Ok(());
            }
            // Each round trip is a bounded progress probe, not a fixed sleep.
        }
    }

    fn lifecycle(&mut self, request: Request, name: &str) -> Result<()> {
        let value = self.call(request)?;
        require(
            value["completed"] == true,
            format!("{name}: incomplete lifecycle request"),
        )?;
        self.checkpoint(name);
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        self.lifecycle(
            Request::Start {
                node: "machine".into(),
                allow_reboot: false,
            },
            "machine-started",
        )
    }

    fn crash_start(&mut self, name: &str) -> Result<()> {
        self.lifecycle(
            Request::Crash {
                node: "machine".into(),
            },
            name,
        )?;
        self.start()
    }

    fn checkpoint(&mut self, name: &str) {
        eprintln!("writer-fence checkpoint: {name}");
    }

    fn token(&mut self, output: &str, name: &str) -> Result<String> {
        let value: Value = serde_json::from_str(output)?;
        let token = value["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("missing nonempty token")?;
        self.assertions.push(name.into());
        Ok(token.into())
    }

    fn condition(&mut self, unit: &str, name: &str) -> Result<()> {
        let output = self.command(
            vec![
                "systemctl".into(),
                "show".into(),
                unit.into(),
                "-p".into(),
                "ConditionResult".into(),
                "--value".into(),
            ],
            true,
            name,
        )?;
        require(output.trim() == "no", format!("{name}: {output}"))
    }

    fn barrier(&mut self, data: &str, name: &str) -> Result<()> {
        self.shell(
            "systemctl start postgresql",
            true,
            &format!("{name}:start-skipped"),
        )?;
        self.shell(
            "systemctl is-active postgresql",
            false,
            &format!("{name}:inactive"),
        )?;
        self.command(
            vec![
                "test".into(),
                "!".into(),
                "-e".into(),
                format!("{data}/postmaster.pid"),
            ],
            true,
            &format!("{name}:no-postmaster"),
        )?;
        self.shell(
            "systemctl start postgresql-setup.service",
            true,
            &format!("{name}:setup-skipped"),
        )?;
        self.condition(
            "postgresql-setup.service",
            &format!("{name}:setup-condition"),
        )
    }
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run(args: Args) -> Result<()> {
    require(args.control_fd >= 0, "invalid inherited control descriptor")?;
    // SAFETY: --control-fd names the socket inherited via pass_fds. Ownership is
    // transferred exactly once here and the Client closes it on completion.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    let mut gate = Gate {
        client: Client::new(stream, Duration::from_secs(COMMAND_SECONDS + 60))?,
        began: Instant::now(),
        assertions: vec![],
    };
    let root = format!("harbor-db-postgres --config {} ", quoted(&args.config));
    let pg = format!("runuser -u postgres -- {root}");
    let query = "runuser -u postgres -- psql -X -w -At -v ON_ERROR_STOP=1 -d postgres ";
    let fault = format!(
        "runuser -u postgres -- env PYTHONPATH={} python3 -B {} ",
        quoted(&args.legacy_python_path),
        quoted(&args.fault_script)
    );
    gate.start()?;
    gate.unit("postgresql.service")?;
    let base = gate
        .shell(
            "readlink -f /run/current-system",
            true,
            "base-generation-resolved",
        )?
        .trim()
        .to_owned();
    require(
        base.starts_with("/nix/store/"),
        "invalid base generation path",
    )?;
    gate.shell(format!("{query}-c 'CREATE ROLE application LOGIN SUPERUSER; CREATE TABLE retained(id int); INSERT INTO retained VALUES (1)'"), true, "superuser-and-acknowledged-row-created")?;
    let identifier = gate
        .shell(
            format!("{query}-c 'SELECT system_identifier FROM pg_control_system()'"),
            true,
            "physical-system-identity-read",
        )?
        .trim()
        .to_owned();
    require(
        !identifier.is_empty() && identifier.bytes().all(|b| b.is_ascii_digit()),
        "invalid physical system identifier",
    )?;
    let output = gate.shell(
        format!("{root}inhibit-startup --system-identifier {identifier}"),
        true,
        "prepare-startup-inhibited",
    )?;
    let held = gate.token(&output, "prepare-inhibition-token")?;
    gate.shell(
        "systemctl stop postgresql",
        true,
        "prepare-postgres-stopped",
    )?;
    let output = gate.shell(
        format!("{fault}prepare {identifier} 2>&1"),
        false,
        "prepare-interruption-rejected",
    )?;
    require(
        output.contains("interrupted before selector publication"),
        "prepare fault checkpoint missing",
    )?;
    gate.assertions
        .push("prepare-interrupted-before-selector-publication".into());
    gate.crash_start("legacy-prepare-crashed")?;
    gate.unit("multi-user.target")?;
    gate.barrier(&args.data_dir, "legacy-prepare-reboot-root-barrier")?;
    let output = gate.shell(
        format!("{pg}fence-open --system-identifier {identifier}"),
        true,
        "fence-open-resumed",
    )?;
    let token = gate.token(&output, "fence-token")?;
    gate.shell(
        format!(
            "{root}release-startup --token {} --fence-token {} --phase prepared",
            quoted(&held),
            quoted(&token)
        ),
        true,
        "prepared-startup-released",
    )?;
    gate.shell(
        "systemctl start postgresql",
        true,
        "prepared-postgres-started",
    )?;
    gate.unit("postgresql.service")?;
    gate.shell(
        format!("{pg}inspect-fence --token {}", quoted(&token)),
        true,
        "prepared-fence-inspected",
    )?;
    gate.shell(
        format!("{query}-U application -c 'INSERT INTO retained VALUES (2)'"),
        false,
        "superuser-write-blocked",
    )?;
    gate.checkpoint("prepared-fence-blocks-superuser");
    gate.crash_start("selected-hba-legacy-crashed")?;
    gate.unit("postgresql.service")?;
    // Joining the boot setup job keeps its transient control SQL out of the
    // exclusive live-session probe. The fence inspection itself stays strict.
    gate.shell(
        "systemctl start postgresql-setup.service",
        true,
        "legacy-reboot-setup-completed",
    )?;
    gate.shell(
        format!("{pg}inspect-fence --token {}", quoted(&token)),
        true,
        "legacy-reboot-fence-inspected",
    )?;
    gate.shell(
        format!("{query}-U application -c 'SELECT 1'"),
        false,
        "legacy-reboot-superuser-connection-blocked",
    )?;
    gate.shell(
        "systemctl stop postgresql",
        true,
        "adoption-postgres-stopped",
    )?;
    gate.shell(
        format!("{pg}adopt --system-identifier {identifier}"),
        true,
        "physical-identity-adopted",
    )?;
    gate.shell(
        format!(
            "{}/specialisation/guarded/bin/switch-to-configuration test",
            quoted(&base)
        ),
        true,
        "guarded-generation-activated",
    )?;
    gate.shell(
        "systemctl start postgresql",
        true,
        "guarded-postgres-started",
    )?;
    gate.unit("postgresql.service")?;
    gate.shell(
        format!("{pg}inspect-fence --token {}", quoted(&token)),
        true,
        "guarded-fence-inspected",
    )?;
    gate.shell(
        "systemctl start fixture-client",
        true,
        "guarded-client-start-skipped",
    )?;
    gate.condition("fixture-client", "guarded-client-condition-blocked")?;
    let conditions = gate.shell(
        "busctl --json=short get-property org.freedesktop.systemd1 /org/freedesktop/systemd1/unit/fixture_2dclient_2eservice org.freedesktop.systemd1.Unit Conditions",
        true,
        "client-conditions-read",
    )?;
    let conditions: Value = serde_json::from_str(&conditions)?;
    let expected = json!(["ConditionPathExists", false, false, "/etc/os-release"]);
    require(
        conditions["type"] == "a(sbbsi)"
            && conditions["data"].as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.as_array().is_some_and(|parts| {
                        parts.len() == 5 && parts[..4] == expected.as_array().unwrap()[..]
                    })
                })
            }),
        format!("existing client condition was lost: {conditions}"),
    )?;
    gate.assertions
        .push("existing-client-condition-preserved".into());
    let count = gate.shell(
        format!("{query}-c 'SELECT count(*) FROM retained'"),
        true,
        "control-sql-live-while-fenced",
    )?;
    require(
        count.trim() == "1",
        format!("fenced acknowledged row count: {count}"),
    )?;
    gate.assertions
        .push("fenced-acknowledged-row-count-one".into());
    let output = gate.shell(
        format!("{root}inhibit-startup --system-identifier {identifier}"),
        true,
        "thaw-startup-inhibited",
    )?;
    let held = gate.token(&output, "thaw-inhibition-token")?;
    gate.shell("systemctl stop postgresql", true, "thaw-postgres-stopped")?;
    let output = gate.shell(
        format!("{fault}thaw {} 2>&1", quoted(&token)),
        false,
        "thaw-interruption-rejected",
    )?;
    require(
        output.contains("interrupted after original selector restoration"),
        "thaw fault checkpoint missing",
    )?;
    gate.assertions
        .push("thaw-interrupted-after-selector-restoration".into());
    let release = format!(
        "{root}release-startup --token {} --fence-token {} --phase closed",
        quoted(&held),
        quoted(&token)
    );
    gate.shell(&release, false, "release-before-closed-rejected")?;
    gate.shell(
        format!("{}/bin/switch-to-configuration test", quoted(&base)),
        true,
        "legacy-generation-restored-during-thaw",
    )?;
    gate.barrier(&args.data_dir, "legacy-generation-thaw-root-barrier")?;
    gate.crash_start("unfinished-thaw-legacy-crashed")?;
    gate.unit("multi-user.target")?;
    gate.barrier(&args.data_dir, "legacy-thaw-reboot-root-barrier")?;
    gate.shell(
        format!("{pg}fence-close --token {}", quoted(&token)),
        true,
        "explicit-thaw-closed",
    )?;
    gate.shell(&release, true, "closed-startup-released")?;
    gate.shell(
        "systemctl start postgresql",
        true,
        "thawed-postgres-started",
    )?;
    gate.unit("postgresql.service")?;
    gate.shell(
        "systemctl start fixture-client",
        true,
        "thawed-client-executed",
    )?;
    let count = gate.shell(
        format!("{query}-c 'SELECT count(*) FROM retained'"),
        true,
        "thawed-row-count-read",
    )?;
    require(
        count.trim() == "2",
        format!("thawed acknowledged row count: {count}"),
    )?;
    gate.assertions
        .push("thawed-acknowledged-row-count-two".into());
    gate.command(
        vec![
            "test".into(),
            "-e".into(),
            format!("{}/lock", args.startup_dir),
        ],
        true,
        "startup-lock-retained",
    )?;
    gate.command(
        vec![
            "test".into(),
            "!".into(),
            "-e".into(),
            format!("{}/inhibited.json", args.startup_dir),
        ],
        true,
        "startup-inhibition-removed",
    )?;
    gate.checkpoint("explicit-thaw-restores-client-and-two-rows");
    require(
        !gate.assertions.is_empty() && gate.assertions.iter().all(|s| !s.is_empty()),
        "empty semantic assertions",
    )?;
    let assertions: Vec<_> = gate
        .assertions
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| json!({"name": name, "passed": true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.acceptance,
        &json!({
            "schema": 1,
            "case_id": harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-postgres-writer-fence"),
            "assertions": assertions,
        }),
    )?;
    Ok(())
}

fn main() -> Result<()> {
    // A wall-clock watchdog also bounds blocking VM Start/Crash transport and
    // acceptance publication. It never participates in progress polling.
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver
            .recv_timeout(Duration::from_secs(LIFECYCLE_SECONDS))
            .is_err()
        {
            eprintln!("writer-fence overall 3600-second lifecycle deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
