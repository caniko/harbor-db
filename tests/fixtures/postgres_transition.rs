//! Rust-owned borrowed-fence transition; the version-1 bridge is VM transport only.
use clap::Parser;
use harbor_db::testing::protocol::{Client, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const OVERALL: u64 = 3600;
const COMMAND: u64 = 900;
const BASE: &str = "/var/lib/demo-primary-backup";
const RECEIPT: &str = "/var/lib/demo-authority/independent.json";

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_fd: i32,
    /// JSON object, not a filename. All package roots and contracts are store paths.
    #[arg(long)]
    config: String,
    #[arg(long)]
    evidence: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    native_package: String,
    storage_package: String,
    postgres_package: String,
    coreutils_package: String,
    shell: String,
    tool_roots: Vec<String>,
    transition_manifest: String,
    backup_manifest: String,
    source_manifest: String,
    target_manifest: String,
}
fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message.into()).into())
    }
}
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn store(s: &str) -> Result<()> {
    let suffix = s
        .strip_prefix("/nix/store/")
        .ok_or("expected immutable store path")?;
    require(
        !suffix.is_empty()
            && !suffix
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.+".contains(&b)),
        "invalid store path",
    )
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing string {key}").into())
}
fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn lsn(s: &str) -> Result<()> {
    let (a, b) = s.split_once('/').ok_or("invalid LSN")?;
    require(
        !a.is_empty()
            && !b.is_empty()
            && u32::from_str_radix(a, 16).is_ok()
            && u32::from_str_radix(b, 16).is_ok(),
        "invalid LSN",
    )
}
struct Gate {
    client: Client,
    began: Instant,
    assertions: BTreeSet<String>,
    shell: String,
    path: String,
}
impl Gate {
    fn check(&mut self, ok: bool, name: &str) -> Result<()> {
        require(ok, name)?;
        require(
            !name.is_empty() && self.assertions.insert(name.into()),
            "empty or duplicate assertion",
        )
    }
    fn call(&mut self, request: Request) -> Result<Value> {
        require(
            self.began.elapsed() < Duration::from_secs(OVERALL),
            "lifecycle deadline exceeded",
        )?;
        Ok(self.client.call(request)?)
    }
    fn execute(&mut self, node: &str, script: &str, seconds: u64) -> Result<(i64, String)> {
        let v = self.call(Request::Execute {
            node: node.into(),
            argv: vec![
                self.shell.clone(),
                "-c".into(),
                format!("set -eu; export PATH={}; {script}", quoted(&self.path)),
            ],
            timeout_seconds: seconds,
        })?;
        Ok((
            v["exit_code"].as_i64().ok_or("missing exit code")?,
            v["output"].as_str().ok_or("missing output")?.into(),
        ))
    }
    fn shell(
        &mut self,
        node: &str,
        script: impl AsRef<str>,
        success: bool,
        name: &str,
    ) -> Result<String> {
        let (code, output) = self.execute(node, script.as_ref(), COMMAND)?;
        require(
            (code == 0) == success && code >= 0,
            format!("{name}: exit {code}: {output}"),
        )?;
        self.check(true, name)?;
        Ok(output.trim().into())
    }
    fn json(&mut self, node: &str, script: impl AsRef<str>, name: &str) -> Result<Value> {
        let output = self.shell(node, script, true, name)?;
        let v: Value = serde_json::from_str(&output)?;
        require(v.is_object(), "expected JSON object")?;
        Ok(v)
    }
    fn unit(&mut self, node: &str, unit: &str, name: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(COMMAND);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            require(!left.is_zero(), format!("unit deadline: {unit}"))?;
            let (code, out) = self.execute(
                node,
                &format!("systemctl is-active {}", quoted(unit)),
                left.as_secs().clamp(1, 60),
            )?;
            if code == 0 && out.trim() == "active" {
                return self.check(true, name);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn recovery_ready(&mut self, postgres: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(COMMAND);
        let script = format!(
            "runuser -u postgres -- {}/bin/psql -Xw -h /var/lib/demo-recovery-socket -p 55432 -Atqc 'SELECT NOT pg_is_in_recovery()'",
            quoted(postgres)
        );
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            require(!left.is_zero(), "recovery promotion deadline exceeded")?;
            let (code, out) = self.execute("certifier", &script, left.as_secs().clamp(1, 60))?;
            if code == 0 && out.trim() == "t" {
                return self.check(true, "independent-recovery-promoted");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn publish(&mut self, node: &str, path: &str, value: &Value, name: &str) -> Result<()> {
        self.shell(
            node,
            format!(
                "printf '%s' {} > {}",
                quoted(&serde_json::to_string(value)?),
                quoted(path)
            ),
            true,
            name,
        )?;
        Ok(())
    }
    fn align(&mut self, receiver: &str, sender: &str, name: &str) -> Result<()> {
        let mut seconds = 0_u64;
        for node in [receiver, sender] {
            let (code, out) = self.execute(node, "date +%s", COMMAND)?;
            require(code == 0, "clock read failed")?;
            seconds = seconds.max(out.trim().parse()?);
        }
        self.shell(receiver, format!("date --set=@{seconds}"), true, name)?;
        Ok(())
    }
    fn transfer(
        &mut self,
        from: &str,
        source: &str,
        to: &str,
        destination: &str,
        host: &PathBuf,
        name: &str,
    ) -> Result<()> {
        let a = self.call(Request::CopyFrom {
            node: from.into(),
            source: source.into(),
            destination: host.to_string_lossy().into(),
        })?;
        require(a["transferred"] == true, "export incomplete")?;
        let b = self.call(Request::CopyTo {
            node: to.into(),
            source: host.to_string_lossy().into(),
            destination: destination.into(),
        })?;
        require(b["transferred"] == true, "import incomplete")?;
        let (source_code, left) =
            self.execute(from, &format!("sha256sum {}", quoted(source)), COMMAND)?;
        let (code, right) =
            self.execute(to, &format!("sha256sum {}", quoted(destination)), COMMAND)?;
        let digest = left
            .split_whitespace()
            .next()
            .ok_or("missing transfer digest")?;
        self.check(
            source_code == 0
                && code == 0
                && hex(digest, 64)
                && right.split_whitespace().next() == Some(digest),
            name,
        )?;
        std::fs::remove_file(host)?;
        Ok(())
    }
    fn imports(&mut self, name: &str) -> Result<()> {
        let out = self.shell(
            "primary",
            "cat /var/lib/demo-new/import-count",
            true,
            &format!("{name}:read"),
        )?;
        self.check(out == "1", name)
    }
    fn phase(&mut self, command: &str, phase: &str, name: &str) -> Result<Value> {
        let v = self.json("primary", command, &format!("{name}:read"))?;
        self.check(v["version"] == 1 && v["phase"] == phase, name)?;
        Ok(v)
    }
}
fn run(args: Args) -> Result<()> {
    let c: Config = serde_json::from_str(&args.config)?;
    for path in [
        &c.native_package,
        &c.storage_package,
        &c.postgres_package,
        &c.coreutils_package,
        &c.shell,
        &c.transition_manifest,
        &c.backup_manifest,
        &c.source_manifest,
        &c.target_manifest,
    ]
    .into_iter()
    .chain(c.tool_roots.iter())
    {
        store(path)?;
    }
    require(
        !c.tool_roots.is_empty()
            && c.native_package == c.storage_package
            && c.source_manifest != c.target_manifest
            && args.control_fd >= 0,
        "invalid fixture configuration",
    )?;
    // SAFETY: pass_fds transfers this inherited socket to exactly one Client.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    let path = c
        .tool_roots
        .iter()
        .chain([
            &c.coreutils_package,
            &c.postgres_package,
            &c.storage_package,
            &c.native_package,
        ])
        .map(|p| format!("{p}/bin"))
        .collect::<Vec<_>>()
        .join(":");
    let mut g = Gate {
        client: Client::new(stream, Duration::from_secs(COMMAND + 60))?,
        began: Instant::now(),
        assertions: BTreeSet::new(),
        shell: c.shell.clone(),
        path,
    };
    for node in ["primary", "certifier"] {
        let v = g.call(Request::Start {
            node: node.into(),
            allow_reboot: false,
        })?;
        g.check(v["completed"] == true, &format!("{node}:started"))?;
        g.unit(node, "multi-user.target", &format!("{node}:booted"))?;
    }
    g.unit("primary", "postgresql.service", "primary:postgres-active")?;
    let control = format!(
        "runuser -u postgres -- {}/bin/psql -XwqAt -v ON_ERROR_STOP=1",
        quoted(&c.postgres_package)
    );
    let sql = |statement: &str| format!("{control} -c {}", quoted(statement));
    let client = format!(
        "runuser -u demo -- {}/bin/psql -Xw -U demo_runtime -d postgres -c 'SELECT * FROM documents'",
        quoted(&c.postgres_package)
    );
    g.shell("primary",sql("CREATE ROLE demo_owner NOLOGIN; CREATE ROLE demo_runtime LOGIN; CREATE TABLE documents(id int PRIMARY KEY, body text); ALTER TABLE documents OWNER TO demo_owner; GRANT SELECT ON documents TO demo_runtime"),true,"schema-created")?;
    g.shell("primary", &client, true, "initial-client-admitted")?;
    let identifier = g.shell(
        "primary",
        sql("SELECT system_identifier FROM pg_control_system()"),
        true,
        "primary-identity-read",
    )?;
    require(
        identifier.parse::<u64>().is_ok_and(|n| n > 0),
        "invalid system identifier",
    )?;
    let mut selected = g.json(
        "primary",
        format!("cat {}", quoted(&c.transition_manifest)),
        "transition-template-read",
    )?;
    g.check(
        selected["source_manifest"] == c.source_manifest
            && selected["target_manifest"] == c.target_manifest
            && selected["storage_package"] == format!("{}/bin", c.storage_package),
        "exact-selected-contracts",
    )?;
    let template = field(&selected, "postgres_manifest")?;
    store(template)?;
    let mut database = g.json(
        "primary",
        format!("cat {}", quoted(template)),
        "postgres-template-read",
    )?;
    g.check(
        database["package"] == c.postgres_package
            && database["major"] == "18"
            && database["recovery"]["backup_root"] == BASE
            && database["recovery"]["require_writer_fence"] == true,
        "exact-postgres-18-recovery-contract",
    )?;
    database["recovery"]["system_identifier"] = json!(identifier);
    g.publish(
        "primary",
        "/run/demo-primary.json",
        &database,
        "resolved-database-published",
    )?;
    let db_path = g.shell(
        "primary",
        "nix-store --add /run/demo-primary.json",
        true,
        "database-added-to-store",
    )?;
    store(&db_path)?;
    selected["postgres_manifest"] = json!(db_path);
    g.publish(
        "primary",
        "/run/demo-transition.json",
        &selected,
        "resolved-transition-published",
    )?;
    let contract = g.shell(
        "primary",
        "nix-store --add /run/demo-transition.json",
        true,
        "transition-added-to-store",
    )?;
    store(&contract)?;
    let pg = format!(
        "runuser -u postgres -- {}/bin/harbor-db-postgres --config {}",
        quoted(&c.storage_package),
        quoted(&db_path)
    );
    let transition = format!(
        "{}/bin/harbor-db-transition --config {}",
        quoted(&c.native_package),
        quoted(&contract)
    );
    let resource = |manifest: &str| {
        format!(
            "runuser -u demo -- {}/bin/harbor-db-resource --config {}",
            quoted(&c.native_package),
            quoted(manifest)
        )
    };
    g.shell(
        "primary",
        format!(
            "runuser -u demo -- {} -c 'printf source-revision-seven > /var/lib/demo-old/records'",
            quoted(&c.shell)
        ),
        true,
        "source-record-created",
    )?;
    g.shell(
        "primary",
        format!(
            "{} adopt --identity retained-resource",
            resource(&c.source_manifest)
        ),
        true,
        "source-authority-adopted",
    )?;
    g.shell(
        "primary",
        "systemctl start demo.service",
        true,
        "source-service-started",
    )?;
    g.unit("primary", "demo.service", "source-service-active")?;
    g.shell(
        "primary",
        "systemctl stop postgresql.service",
        true,
        "primary-stopped-for-fence",
    )?;
    let opened = g.json(
        "primary",
        format!(
            "{pg} fence-open --system-identifier {}",
            quoted(&identifier)
        ),
        "fence-opened",
    )?;
    let token = field(&opened, "token")?.to_owned();
    require(hex(&token, 32), "invalid fence token")?;
    let inspect = format!("{pg} inspect-fence --token {}", quoted(&token));
    g.shell("primary", &inspect, false, "stopped-fence-not-live")?;
    g.shell(
        "primary",
        "systemctl start postgresql.service",
        true,
        "fenced-primary-started",
    )?;
    g.unit("primary", "postgresql.service", "fenced-primary-active")?;
    g.shell("primary", &inspect, true, "same-fence-live")?;
    let source_fence = g.json(
        "primary",
        "cat /var/lib/demo-primary-authority/writer-fence.json",
        "source-fence-authentication-selector-read",
    )?;
    g.check(
        source_fence["token"] == token,
        "source-fence-authentication-binds-borrowed-token",
    )?;
    let source_hba = field(&source_fence, "hba_file")?.to_owned();
    g.shell("primary", &client, false, "fence-denies-client")?;
    let planned = g.phase(
        &format!(
            "{transition} plan --candidate {} --writer-fence-token {}",
            quoted(&contract),
            quoted(&token)
        ),
        "planned",
        "transition-planned",
    )?;
    g.check(
        planned["writer_fence_token"] == token,
        "plan-borrows-exact-fence",
    )?;
    let captured = g.phase(
        &format!("{transition} prepare"),
        "captured",
        "application-captured",
    )?;
    g.check(
        captured["status"] == "awaiting-independent-restore",
        "independent-restore-required",
    )?;
    g.shell(
        "primary",
        "systemctl is-active demo.service",
        false,
        "captured-service-inactive",
    )?;
    g.shell(
        "primary",
        "systemctl start demo.service",
        true,
        "captured-start-skipped",
    )?;
    g.shell(
        "primary",
        "systemctl is-active demo.service",
        false,
        "captured-service-inhibited",
    )?;
    g.shell(
        "primary",
        format!("{} check", resource(&c.source_manifest)),
        false,
        "captured-source-admission-denied",
    )?;
    let backup = field(&captured, "backup")?;
    let point = backup
        .strip_prefix("/var/lib/demo-backups/")
        .ok_or("backup escaped root")?;
    require(
        !point.is_empty()
            && point
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid backup point",
    )?;
    let host = args.evidence.with_extension("transfer.tar");
    g.shell(
        "primary",
        format!(
            "tar -C /var/lib/demo-backups -cf /run/application.tar {}",
            quoted(point)
        ),
        true,
        "application-archive-created",
    )?;
    g.transfer(
        "primary",
        "/run/application.tar",
        "certifier",
        "/run/application.tar",
        &host,
        "application-archive-byte-equality",
    )?;
    g.shell(
        "certifier",
        "tar -xf /run/application.tar -C /var/lib/demo-backups",
        true,
        "application-restored-on-certifier",
    )?;
    g.align(
        "certifier",
        "primary",
        "application-certifier-clock-aligned",
    )?;
    let proof = g.json("certifier",format!("runuser -u demo -- {}/bin/harbor-db-application-backup --config {} certify {} --state /var/lib/demo-certifier",quoted(&c.native_package),quoted(&c.backup_manifest),quoted(backup)),"independent-application-certification")?;
    let digest = field(&proof, "source_acceptance_sha256")?;
    g.check(
        hex(digest, 64)
            && captured["source_acceptance_sha256"] == digest
            && proof["version"] == 1
            && proof["status"] == "verified",
        "application-proof-binds-captured-source",
    )?;
    g.transfer(
        "certifier",
        &format!("/var/lib/demo-certifier/{digest}.json"),
        "primary",
        RECEIPT,
        &host,
        "application-receipt-byte-equality",
    )?;
    g.align("primary", "certifier", "application-primary-clock-aligned")?;
    g.shell(
        "primary",
        format!("{transition} prepare"),
        false,
        "import-retained-awaiting-primary-recovery",
    )?;
    g.phase(
        &format!("{transition} status"),
        "imported",
        "imported-phase-retained",
    )?;
    g.imports("first-import-exactly-once")?;
    g.shell(
        "primary",
        format!("{transition} prepare"),
        false,
        "missing-physical-proof-retry-rejected",
    )?;
    g.imports("missing-proof-retry-no-reimport")?;
    let authority = g.json(
        "primary",
        "cat /var/lib/demo-authority/identity.json",
        "old-authority-read",
    )?;
    g.check(
        authority["binding"]["backend"] == "filesystem-source",
        "old-authority-remains-published",
    )?;
    g.shell(
        "primary",
        format!("{} check", resource(&c.target_manifest)),
        false,
        "target-admission-denied-before-recovery",
    )?;
    g.shell("primary", &client, false, "import-does-not-thaw-client")?;
    g.shell("primary",format!("runuser -u postgres -- {}/bin/pg_basebackup -h /run/postgresql -U postgres -D {BASE}/base/base-1 -X stream --checkpoint=fast",quoted(&c.postgres_package)),true,"actual-physical-basebackup")?;
    let manifest = g.json(
        "primary",
        format!("cat {BASE}/base/base-1/backup_manifest"),
        "physical-manifest-read",
    )?;
    require(
        manifest["WAL-Ranges"]
            .as_array()
            .is_some_and(|r| r.len() == 1),
        "expected exactly one physical WAL range",
    )?;
    let stop = field(&manifest["WAL-Ranges"][0], "End-LSN")?;
    lsn(stop)?;
    let target = g.shell(
        "primary",
        sql("SELECT pg_create_restore_point('transition_import')"),
        true,
        "post-import-restore-point",
    )?;
    lsn(&target)?;
    g.shell(
        "primary",
        sql("SELECT pg_switch_wal()"),
        true,
        "physical-wal-switched",
    )?;
    g.publish("primary",&format!("{BASE}/base/base-1.meta.json"),&json!({"backup_id":"base-1","system_identifier":identifier,"pg_major":18,"epoch_id":"import","backup_stop_lsn":stop,"post_backup_lsn":target}),"physical-metadata-published")?;
    g.shell(
        "primary",
        format!("printf base-1 > {BASE}/LAST_SUCCESS; chown -R postgres:postgres {BASE}"),
        true,
        "physical-backup-selected",
    )?;
    let snapshot = g.json(
        "primary",
        format!("{pg} snapshot-records --socket-dir /run/postgresql --port 5432"),
        "real-primary-records-snapshotted",
    )?;
    g.check(
        snapshot["version"] == 1
            && snapshot["writer_fence_token"] == token
            && snapshot["records"]
                .as_object()
                .is_some_and(|r| r.len() == 1)
            && hex(field(&snapshot["records"], "documents")?, 64),
        "physical-snapshot-binds-borrowed-fence",
    )?;
    g.shell("primary", "cp /var/lib/postgres/18/pg_wal/0000000* /var/lib/demo-restore-wal/ && tar -C /var/lib -cf /run/physical.tar demo-primary-backup demo-restore-wal", true, "physical-recovery-archive-created")?;
    g.transfer(
        "primary",
        "/run/physical.tar",
        "certifier",
        "/run/physical.tar",
        &host,
        "physical-backup-wal-snapshot-byte-equality",
    )?;
    g.shell("certifier","tar -xf /run/physical.tar -C /var/lib && cp -a /var/lib/demo-primary-backup/base/base-1 /var/lib/demo-recovered/18 && chown -R postgres:postgres /var/lib/demo-primary-backup /var/lib/demo-recovered /var/lib/demo-restore-wal",true,"physical-copy-restored-on-independent-host")?;
    g.publish(
        "certifier",
        "/run/demo-primary.json",
        &database,
        "certifier-database-contract-published",
    )?;
    let independent_db = g.shell(
        "certifier",
        "nix-store --add /run/demo-primary.json",
        true,
        "certifier-database-contract-immutable",
    )?;
    g.check(
        independent_db == db_path,
        "certifier-uses-identical-database-contract",
    )?;
    let restored = format!(
        "listen_addresses = ''\nunix_socket_directories = '/var/lib/demo-recovery-socket'\nport = 55432\nrestore_command = '{}/bin/cp /var/lib/demo-restore-wal/%f %p'\nrecovery_target_lsn = '{target}'\nrecovery_target_action = 'promote'\ndefault_transaction_read_only = on\n",
        c.coreutils_package
    );
    g.shell("certifier",format!("printf '%s' {} > /var/lib/demo-recovered/18/postgresql.conf; touch /var/lib/demo-recovered/18/recovery.signal; chown postgres:postgres /var/lib/demo-recovered/18/postgresql.conf /var/lib/demo-recovered/18/recovery.signal",quoted(&restored)),true,"disposable-recovery-configured")?;
    let restored_auto = g.shell(
        "certifier",
        format!(
            "test ! -e {}; sha256sum /var/lib/demo-recovered/18/postgresql.auto.conf",
            quoted(&source_hba)
        ),
        true,
        "source-fence-authentication-is-absent-on-independent-host",
    )?;
    // Reproduce the original failed topology before rebinding the disposable
    // endpoint. The retained server error must identify the missing source HBA,
    // rather than accepting a startup rejection for an unrelated cause.
    g.shell(
        "certifier",
        format!("runuser -u postgres -- {}/bin/pg_ctl -p {}/bin/postgres -D /var/lib/demo-recovered/18 -l /var/lib/demo-recovered/original-config.log -w start", quoted(&c.postgres_package), quoted(&c.postgres_package)),
        false,
        "source-fence-authentication-cannot-start-on-independent-host",
    )?;
    let startup_error = g.shell(
        "certifier",
        "test ! -e /var/lib/demo-recovered/18/postmaster.pid; cat /var/lib/demo-recovered/original-config.log",
        true,
        "original-startup-error-read-without-live-postmaster",
    )?;
    g.check(
        startup_error.contains(&source_hba) && startup_error.contains("No such file or directory"),
        "original-startup-error-identifies-missing-source-hba",
    )?;
    // The backup retains the primary's absolute fence HBA selector. Only the
    // disposable copy uses these private peer-auth files and command overrides.
    g.shell(
        "certifier",
        "printf 'local all postgres peer\\n' > /var/lib/demo-recovered/pg_hba.conf; : > /var/lib/demo-recovered/pg_ident.conf; chown postgres:postgres /var/lib/demo-recovered/pg_hba.conf /var/lib/demo-recovered/pg_ident.conf; chmod 0600 /var/lib/demo-recovered/pg_hba.conf /var/lib/demo-recovered/pg_ident.conf",
        true,
        "disposable-private-peer-auth-configured",
    )?;
    g.shell(
        "certifier",
        format!(
            "runuser -u postgres -- {}/bin/pg_ctl -p {}/bin/postgres -D /var/lib/demo-recovered/18 -o {} -l /var/lib/demo-recovered/server.log -w start || {{ cat /var/lib/demo-recovered/server.log; exit 1; }}",
            quoted(&c.postgres_package),
            quoted(&c.postgres_package),
            quoted("-c data_directory=/var/lib/demo-recovered/18 -c listen_addresses= -c hba_file=/var/lib/demo-recovered/pg_hba.conf -c ident_file=/var/lib/demo-recovered/pg_ident.conf"),
        ),
        true,
        "physical-recovery-started",
    )?;
    g.recovery_ready(&c.postgres_package)?;
    let endpoint = g.json(
        "certifier",
        format!(
            "runuser -u postgres -- {}/bin/psql -Xw -h /var/lib/demo-recovery-socket -p 55432 -Atqc {}",
            quoted(&c.postgres_package),
            quoted("SELECT json_build_object('hba_file', current_setting('hba_file'), 'socket', current_setting('unix_socket_directories'), 'tcp', current_setting('listen_addresses'), 'read_only', current_setting('default_transaction_read_only'))"),
        ),
        "disposable-endpoint-policy-read",
    )?;
    g.check(
        endpoint == json!({"hba_file":"/var/lib/demo-recovered/pg_hba.conf","socket":"/var/lib/demo-recovery-socket","tcp":"","read_only":"on"}),
        "disposable-endpoint-is-private-and-read-only",
    )?;
    let rebound_auto = g.shell(
        "certifier",
        "sha256sum /var/lib/demo-recovered/18/postgresql.auto.conf",
        true,
        "rebound-disposable-auto-config-read",
    )?;
    g.check(
        rebound_auto == restored_auto,
        "disposable-rebinding-preserves-copied-auto-config",
    )?;
    g.align("certifier", "primary", "physical-certifier-clock-aligned")?;
    let recovery = g.json("certifier",format!("{pg} certify-recovery --data-dir /var/lib/demo-recovered/18 --socket-dir /var/lib/demo-recovery-socket --port 55432"),"independent-physical-recovery-certified")?;
    g.check(
        recovery["status"] == "ready"
            && recovery["executor_host"] == "certifier"
            && recovery["records"] == snapshot["records"],
        "whole-primary-real-independent-record-proof",
    )?;
    g.transfer(
        "certifier",
        &format!("{BASE}/evidence/recovery.json"),
        "primary",
        "/run/recovery.json",
        &host,
        "physical-receipt-byte-equality",
    )?;
    g.shell("primary",format!("cp /run/recovery.json {BASE}/evidence/recovery.json && chown postgres:postgres {BASE}/evidence/recovery.json"),true,"physical-receipt-installed")?;
    g.align("primary", "certifier", "physical-primary-clock-aligned")?;
    g.phase(
        &format!("{transition} prepare"),
        "prepared",
        "whole-primary-gates-prepared",
    )?;
    g.imports("prepared-no-reimport")?;
    g.shell(
        "primary",
        format!("{pg} inspect-recovery --socket-dir /run/postgresql --port 5432"),
        true,
        "live-primary-recovery-accepted",
    )?;
    g.shell(
        "primary",
        sql("UPDATE documents SET body='lost-review'"),
        true,
        "same-count-record-drift-injected",
    )?;
    let count = g.shell(
        "primary",
        sql("SELECT count(*) FROM documents"),
        true,
        "drift-row-count-read",
    )?;
    g.check(count == "1", "drift-preserves-exact-row-count")?;
    g.shell(
        "primary",
        format!("{pg} inspect-recovery --socket-dir /run/postgresql --port 5432"),
        false,
        "record-drift-recovery-rejected",
    )?;
    g.shell(
        "primary",
        format!("{transition} prepare"),
        false,
        "record-drift-transition-rejected",
    )?;
    g.imports("drift-rejection-no-reimport")?;
    g.shell(
        "primary",
        sql("UPDATE documents SET body='source-revision-seven'"),
        true,
        "original-records-restored",
    )?;
    g.phase(
        &format!("{transition} prepare"),
        "prepared",
        "retry-prepared",
    )?;
    g.imports("retry-no-reimport")?;
    g.shell(
        "primary",
        format!("{transition} abort"),
        true,
        "transition-aborted",
    )?;
    g.shell(
        "primary",
        format!("{} check", resource(&c.source_manifest)),
        true,
        "abort-source-authority-restored",
    )?;
    g.shell("primary", &inspect, true, "abort-retains-borrowed-fence")?;
    g.shell("primary", &client, false, "abort-client-still-denied")?;
    g.shell(
        "primary",
        format!("{transition} retire"),
        true,
        "aborted-transition-retired",
    )?;
    g.shell(
        "primary",
        &inspect,
        true,
        "retirement-retains-borrowed-fence",
    )?;
    g.shell("primary", &client, false, "retirement-client-still-denied")?;
    g.shell(
        "certifier",
        format!(
            "runuser -u postgres -- {}/bin/pg_ctl -D /var/lib/demo-recovered/18 -w stop",
            quoted(&c.postgres_package)
        ),
        true,
        "disposable-recovery-stopped",
    )?;
    g.shell(
        "primary",
        "systemctl stop postgresql.service",
        true,
        "primary-explicitly-stopped-for-thaw",
    )?;
    g.shell(
        "primary",
        format!("{pg} fence-close --token {}", quoted(&token)),
        true,
        "explicit-stopped-fence-thaw",
    )?;
    g.shell(
        "primary",
        "systemctl start postgresql.service",
        true,
        "thawed-primary-started",
    )?;
    g.unit("primary", "postgresql.service", "thawed-primary-active")?;
    g.shell(
        "primary",
        &client,
        true,
        "client-available-only-after-explicit-thaw",
    )?;
    g.imports("terminal-import-count-one")?;
    require(!g.assertions.is_empty(), "no semantic assertions")?;
    let assertions: Vec<_> = g
        .assertions
        .into_iter()
        .map(|name| json!({"name":name,"passed":true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.evidence,
        &json!({"schema":1,"case_id":harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-application-postgres-transition"),"assertions":assertions}),
    )?;
    Ok(())
}
fn main() -> Result<()> {
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(OVERALL)).is_err() {
            eprintln!("postgres-transition overall deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
