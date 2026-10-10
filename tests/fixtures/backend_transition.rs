//! Rust-owned filesystem transition lifecycle, with retained Python journal peers.
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
const JOURNAL: &str = "/var/lib/demo-authority/transition.json";
const RECEIPT: &str = "/var/lib/demo-authority/independent.json";

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_fd: i32,
    #[arg(long)]
    source_manifest: String,
    #[arg(long)]
    target_manifest: String,
    #[arg(long)]
    native_package: String,
    #[arg(long)]
    legacy_package: String,
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

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Gate {
    client: Client,
    began: Instant,
    assertions: BTreeSet<String>,
}

impl Gate {
    fn call(&mut self, request: Request) -> Result<Value> {
        require(
            self.began.elapsed() < Duration::from_secs(LIFECYCLE_SECONDS),
            "backend-transition lifecycle budget exhausted",
        )?;
        Ok(self.client.call(request)?)
    }

    fn check(&mut self, ok: bool, name: &str) -> Result<()> {
        require(ok, name)?;
        require(!name.is_empty(), "empty assertion name")?;
        require(
            self.assertions.insert(name.into()),
            format!("duplicate assertion: {name}"),
        )?;
        eprintln!("backend-transition checkpoint: {name}");
        Ok(())
    }

    fn execute(&mut self, node: &str, script: &str, seconds: u64) -> Result<(i64, String)> {
        let result = self.call(Request::Execute {
            node: node.into(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            timeout_seconds: seconds,
        })?;
        Ok((
            result["exit_code"].as_i64().ok_or("missing exit_code")?,
            result["output"].as_str().ok_or("missing output")?.into(),
        ))
    }

    fn shell(
        &mut self,
        node: &str,
        script: impl AsRef<str>,
        success: bool,
        name: &str,
    ) -> Result<String> {
        let (code, output) = self.execute(node, script.as_ref(), COMMAND_SECONDS)?;
        require(
            (code == 0) == success,
            format!("{name}: exit {code}: {output}"),
        )?;
        self.check(true, name)?;
        Ok(output)
    }

    fn phase(&mut self, command: &str, phase: &str, name: &str) -> Result<Value> {
        let (code, output) = self.execute("primary", command, COMMAND_SECONDS)?;
        require(code == 0, format!("{name}: exit {code}: {output}"))?;
        let record: Value = serde_json::from_str(&output)?;
        self.check(record["version"] == 1 && record["phase"] == phase, name)?;
        Ok(record)
    }

    fn lifecycle(&mut self, request: Request, name: &str) -> Result<()> {
        let value = self.call(request)?;
        self.check(value["completed"] == true, name)
    }

    fn start(&mut self, node: &str, name: &str) -> Result<()> {
        self.lifecycle(
            Request::Start {
                node: node.into(),
                allow_reboot: false,
            },
            name,
        )
    }

    fn unit(&mut self, node: &str, unit: &str, name: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECONDS);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            require(
                !remaining.is_zero(),
                format!("waiting for {node}:{unit} timed out"),
            )?;
            let (code, output) = self.execute(
                node,
                &format!("systemctl is-active {}", quoted(unit)),
                remaining.as_secs().clamp(1, 60),
            )?;
            if code == 0 && output.trim() == "active" {
                return self.check(true, name);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn barrier(&mut self, name: &str) -> Result<()> {
        self.shell(
            "primary",
            "systemctl start demo.service",
            true,
            &format!("{name}:start-skipped"),
        )?;
        self.shell(
            "primary",
            "systemctl is-active demo.service",
            false,
            &format!("{name}:inactive"),
        )?;
        let output = self.shell(
            "primary",
            "systemctl show demo.service -p ConditionResult --value",
            true,
            &format!("{name}:condition-read"),
        )?;
        self.check(output.trim() == "no", &format!("{name}:condition-blocked"))
    }

    fn bytes(&mut self, path: &str) -> Result<String> {
        let (code, output) = self.execute(
            "primary",
            &format!("base64 -w0 {}", quoted(path)),
            COMMAND_SECONDS,
        )?;
        require(
            code == 0 && !output.is_empty(),
            format!("cannot read {path}: {output}"),
        )?;
        Ok(output)
    }

    fn readbacks(
        &mut self,
        native: &str,
        python: &str,
        phase: &str,
        name: &str,
        receipt: bool,
    ) -> Result<Value> {
        let journal_before = self.bytes(JOURNAL)?;
        let receipt_before = if receipt {
            Some(self.bytes(RECEIPT)?)
        } else {
            None
        };
        let native_record = self.phase(
            &format!("{native} status"),
            phase,
            &format!("{name}:native-phase"),
        )?;
        let python_record = self.phase(
            &format!("{python} status"),
            phase,
            &format!("{name}:python-phase"),
        )?;
        self.check(
            native_record == python_record,
            &format!("{name}:peer-record-equality"),
        )?;
        let journal_after = self.bytes(JOURNAL)?;
        self.check(
            journal_before == journal_after,
            &format!("{name}:journal-byte-preservation"),
        )?;
        if let Some(before) = receipt_before {
            let after = self.bytes(RECEIPT)?;
            self.check(
                before == after,
                &format!("{name}:receipt-byte-preservation"),
            )?;
        }
        Ok(native_record)
    }

    fn align(&mut self, receiver: &str, sender: &str, name: &str) -> Result<()> {
        let mut seconds = 0_u64;
        for node in [receiver, sender] {
            let (code, output) = self.execute(node, "date +%s", COMMAND_SECONDS)?;
            require(code == 0, "clock read failed")?;
            seconds = seconds.max(output.trim().parse()?);
        }
        self.shell(receiver, format!("date --set=@{seconds}"), true, name)?;
        Ok(())
    }

    fn imports(&mut self, name: &str) -> Result<()> {
        let (code, output) = self.execute(
            "primary",
            "cat /var/lib/demo-new/import-count",
            COMMAND_SECONDS,
        )?;
        self.check(code == 0 && output.trim() == "1", name)
    }
}

fn run(args: Args) -> Result<()> {
    require(args.control_fd >= 0, "invalid inherited control descriptor")?;
    // SAFETY: the inherited socket descriptor is transferred exactly once to Client.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    let mut gate = Gate {
        client: Client::new(stream, Duration::from_secs(COMMAND_SECONDS + 60))?,
        began: Instant::now(),
        assertions: BTreeSet::new(),
    };
    gate.start("primary", "primary-started")?;
    gate.start("certifier", "certifier-started")?;
    gate.unit("primary", "multi-user.target", "primary-booted")?;
    gate.unit("certifier", "multi-user.target", "certifier-booted")?;
    gate.shell(
        "primary",
        "runuser -u demo -- sh -c 'printf source-revision-seven > /var/lib/demo-old/records'",
        true,
        "source-record-created",
    )?;
    gate.shell("primary", format!("runuser -u demo -- {}/bin/harbor-db-resource --config {} adopt --identity retained-resource", quoted(&args.native_package), quoted(&args.source_manifest)), true, "source-authority-adopted")?;
    gate.shell(
        "primary",
        "systemctl start demo.service",
        true,
        "source-writer-started",
    )?;
    gate.unit("primary", "demo.service", "source-writer-active")?;
    let contract = gate
        .shell(
            "primary",
            "readlink -f /etc/harbor-db/demo-transition.json",
            true,
            "immutable-contract-resolved",
        )?
        .trim()
        .to_owned();
    gate.check(
        contract.starts_with("/nix/store/"),
        "immutable-contract-store-path",
    )?;
    let native = format!(
        "{}/bin/harbor-db-transition --config {}",
        quoted(&args.native_package),
        quoted(&contract)
    );
    let python = format!(
        "{}/bin/harbor-db-transition --config {}",
        quoted(&args.legacy_package),
        quoted(&contract)
    );
    gate.phase(
        &format!("{native} plan --candidate {}", quoted(&contract)),
        "planned",
        "native-plan-planned",
    )?;
    gate.readbacks(
        &native,
        &python,
        "planned",
        "native-plan-python-status",
        false,
    )?;
    let captured = gate.phase(
        &format!("{python} prepare"),
        "captured",
        "python-prepare-captured",
    )?;
    gate.check(
        captured["status"] == "awaiting-independent-restore",
        "python-awaiting-independent-restore",
    )?;
    gate.readbacks(
        &native,
        &python,
        "captured",
        "python-capture-native-status",
        false,
    )?;
    gate.shell(
        "primary",
        "systemctl is-active demo.service",
        false,
        "capture-writer-inactive",
    )?;
    gate.barrier("captured-barrier")?;
    gate.shell(
        "primary",
        format!(
            "runuser -u demo -- {}/bin/harbor-db-resource --config {} check",
            quoted(&args.native_package),
            quoted(&args.source_manifest)
        ),
        false,
        "captured-source-check-denied",
    )?;
    gate.shell(
        "primary",
        format!("{native} enable-writes"),
        false,
        "captured-native-thaw-denied",
    )?;
    gate.shell(
        "primary",
        format!("{python} enable-writes"),
        false,
        "captured-python-thaw-denied",
    )?;
    gate.lifecycle(
        Request::Crash {
            node: "primary".into(),
        },
        "captured-primary-crashed",
    )?;
    gate.start("primary", "captured-primary-restarted")?;
    gate.unit("primary", "multi-user.target", "captured-primary-rebooted")?;
    gate.barrier("reboot-barrier")?;
    let resumed = gate.readbacks(&native, &python, "captured", "reboot-readbacks", false)?;
    let mut expected_captured = captured.clone();
    expected_captured
        .as_object_mut()
        .ok_or("capture is not an object")?
        .remove("status");
    gate.check(
        resumed == expected_captured,
        "reboot-captured-record-preserved",
    )?;
    let backup = captured["backup"].as_str().ok_or("missing backup path")?;
    let point = backup
        .strip_prefix("/var/lib/demo-backups/")
        .ok_or("backup escaped root")?;
    gate.check(
        !point.is_empty()
            && !point.contains('/')
            && point
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "backup-point-safe",
    )?;
    let archive = gate.shell(
        "primary",
        format!(
            "tar -C /var/lib/demo-backups -czf - {} | base64 -w0",
            quoted(point)
        ),
        true,
        "backup-exported",
    )?;
    gate.check(!archive.trim().is_empty(), "backup-export-nonempty")?;
    gate.shell(
        "certifier",
        format!(
            "printf '%s' {} | base64 -d | tar -xzf - -C /var/lib/demo-backups",
            quoted(archive.trim())
        ),
        true,
        "backup-transferred-to-independent-certifier",
    )?;
    gate.align("certifier", "primary", "certifier-clock-aligned")?;
    let backup_contract = gate
        .shell(
            "certifier",
            "readlink -f /etc/harbor-db/demo-backup.json",
            true,
            "certifier-contract-resolved",
        )?
        .trim()
        .to_owned();
    let proof = gate.shell("certifier", format!("runuser -u demo -- {}/bin/harbor-db-application-backup --config {} certify {} --state /var/lib/demo-certifier", quoted(&args.native_package), quoted(&backup_contract), quoted(backup)), true, "independent-certification-executed")?;
    let proof: Value = serde_json::from_str(&proof)?;
    let digest = proof["source_acceptance_sha256"]
        .as_str()
        .ok_or("missing acceptance digest")?;
    gate.check(
        digest.len() == 64
            && digest.bytes().all(|b| b.is_ascii_hexdigit())
            && captured["source_acceptance_sha256"] == digest,
        "independent-proof-binds-source-acceptance",
    )?;
    let encoded = gate.shell(
        "certifier",
        format!("base64 -w0 /var/lib/demo-certifier/{digest}.json"),
        true,
        "independent-receipt-exported",
    )?;
    gate.shell(
        "primary",
        format!(
            "printf '%s' {} | base64 -d > {RECEIPT}",
            quoted(encoded.trim())
        ),
        true,
        "independent-receipt-imported",
    )?;
    let transferred = gate.bytes(RECEIPT)?;
    gate.check(
        transferred.trim() == encoded.trim(),
        "independent-receipt-transfer-byte-equality",
    )?;
    gate.align("primary", "certifier", "primary-clock-aligned")?;
    let prepared = gate.phase(
        &format!("{native} prepare"),
        "prepared",
        "native-resumes-python-capture-prepared",
    )?;
    gate.imports("native-resume-imported-once")?;
    let repeated = gate.phase(
        &format!("{python} prepare"),
        "prepared",
        "python-resumes-native-prepared",
    )?;
    gate.check(prepared == repeated, "prepared-peer-resume-record-equality")?;
    gate.imports("python-resume-no-repeated-import")?;
    gate.readbacks(&native, &python, "prepared", "prepared-readbacks", true)?;
    gate.shell(
        "primary",
        format!("{python} enable-writes"),
        false,
        "prepared-python-thaw-denied",
    )?;
    gate.barrier("prepared-barrier")?;
    let candidate = gate
        .shell(
            "primary",
            "readlink -f /run/current-system/specialisation/target",
            true,
            "target-generation-resolved",
        )?
        .trim()
        .to_owned();
    gate.check(
        candidate.starts_with("/nix/store/"),
        "target-generation-store-path",
    )?;
    let bound = gate.phase(
        &format!("{native} bind-candidate --candidate {}", quoted(&candidate)),
        "prepared",
        "native-target-generation-bound",
    )?;
    gate.check(
        bound["candidate"] == candidate,
        "exact-target-generation-bound",
    )?;
    gate.readbacks(&native, &python, "prepared", "bound-readbacks", true)?;
    gate.shell(
        "primary",
        format!("{}/bin/switch-to-configuration test", quoted(&candidate)),
        true,
        "target-generation-activated",
    )?;
    gate.shell(
        "primary",
        "systemctl is-active demo.service",
        false,
        "activated-writer-inactive",
    )?;
    gate.phase(
        &format!("{python} commit"),
        "committed",
        "python-commits-native-bound-journal",
    )?;
    gate.readbacks(&native, &python, "committed", "committed-readbacks", true)?;
    gate.shell(
        "primary",
        "test -f /var/lib/harbor-db-transitions/demo/inhibited.json",
        true,
        "committed-inhibition-retained",
    )?;
    gate.barrier("committed-barrier")?;
    gate.phase(
        &format!("{native} enable-writes"),
        "write-enabled",
        "native-enables-python-commit",
    )?;
    gate.readbacks(
        &native,
        &python,
        "write-enabled",
        "write-enabled-readbacks",
        true,
    )?;
    gate.shell(
        "primary",
        "test ! -e /var/lib/harbor-db-transitions/demo/inhibited.json",
        true,
        "authorized-thaw-inhibition-removed",
    )?;
    gate.shell(
        "primary",
        "systemctl start demo.service",
        true,
        "target-writer-started",
    )?;
    gate.unit("primary", "demo.service", "target-writer-active")?;
    gate.phase(
        &format!("{python} complete"),
        "complete",
        "python-completes-native-write-enabled",
    )?;
    let complete = gate.readbacks(&native, &python, "complete", "complete-readbacks", true)?;
    gate.shell(
        "primary",
        format!("{native} abort"),
        false,
        "native-abort-after-acknowledged-writes-denied",
    )?;
    gate.shell(
        "primary",
        format!("{python} abort"),
        false,
        "python-abort-after-acknowledged-writes-denied",
    )?;
    gate.shell(
        "primary",
        format!(
            "runuser -u demo -- {}/bin/harbor-db-resource --config {} check",
            quoted(&args.native_package),
            quoted(&args.target_manifest)
        ),
        true,
        "target-authority-check-accepted",
    )?;
    gate.shell(
        "primary",
        format!(
            "runuser -u demo -- {}/bin/harbor-db-resource --config {} check",
            quoted(&args.native_package),
            quoted(&args.source_manifest)
        ),
        false,
        "source-authority-check-rejected",
    )?;
    gate.imports("terminal-import-count-one")?;
    let retired = gate.shell(
        "primary",
        format!("{native} retire"),
        true,
        "native-retirement-executed",
    )?;
    let retired: Value = serde_json::from_str(&retired)?;
    gate.check(retired["status"] == "retired", "native-retirement-status")?;
    let history = retired["history"]
        .as_str()
        .ok_or("missing terminal history")?;
    let history_bytes = gate.shell(
        "primary",
        format!("cat {}", quoted(history)),
        true,
        "retained-history-read",
    )?;
    gate.check(
        serde_json::from_str::<Value>(&history_bytes)? == complete,
        "retained-history-exact-terminal-record",
    )?;
    gate.shell(
        "primary",
        format!("test ! -e {JOURNAL}"),
        true,
        "retired-journal-removed",
    )?;
    gate.shell(
        "primary",
        "test -f /var/lib/demo-old/records",
        true,
        "retired-source-records-retained",
    )?;
    gate.shell(
        "primary",
        "grep acknowledged /var/lib/demo-new/records",
        true,
        "target-acknowledged-records-retained",
    )?;
    let (code, records) =
        gate.execute("primary", "cat /var/lib/demo-new/records", COMMAND_SECONDS)?;
    gate.check(
        code == 0 && records == "acknowledged target revision eight",
        "exact-acknowledged-target-revision",
    )?;
    require(!gate.assertions.is_empty(), "empty acceptance assertions")?;
    let assertions: Vec<_> = gate
        .assertions
        .into_iter()
        .map(|name| json!({"name": name, "passed": true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.acceptance,
        &json!({
            "schema": 1,
            "case_id": harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-application-transition"),
            "assertions": assertions,
        }),
    )?;
    Ok(())
}

fn main() -> Result<()> {
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver
            .recv_timeout(Duration::from_secs(LIFECYCLE_SECONDS))
            .is_err()
        {
            eprintln!("backend-transition overall 3600-second lifecycle deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
