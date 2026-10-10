//! Two real hosts: generated source-local backup, retained fence, independent replay.
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
const ROOT: &str = "/srv/pgbackup/127.0.0.1";
const CONFIG: &str = "/run/source-local-primary.json";
const RECORDS: &str =
    "SELECT id,body,revision,encode(payload,'hex') FROM recovery_records ORDER BY id";
const EXPECTED: &str = "1|改訂 café|8|00ff1020\n3|new 🐘|2|deadbeef";
#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: String,
    #[arg(long)]
    control_fd: i32,
    #[arg(long)]
    acceptance: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    native_package: String,
    legacy_package: String,
    postgres_package: String,
    coreutils_package: String,
    shell: String,
    tool_roots: Vec<String>,
}
fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message.into()).into())
    }
}
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing {key}").into())
}
struct Gate {
    client: Client,
    shell: String,
    path: String,
    began: Instant,
    assertions: BTreeSet<String>,
}
impl Gate {
    fn check(&mut self, ok: bool, name: &str) -> Result<()> {
        require(ok, name)?;
        require(self.assertions.insert(name.into()), "duplicate assertion")
    }
    fn call(&mut self, request: Request) -> Result<Value> {
        require(
            self.began.elapsed() < Duration::from_secs(3600),
            "overall deadline",
        )?;
        Ok(self.client.call(request)?)
    }
    fn exec(&mut self, node: &str, script: &str) -> Result<(i64, String)> {
        let v = self.call(Request::Execute {
            node: node.into(),
            argv: vec![
                self.shell.clone(),
                "-c".into(),
                format!("set -euo pipefail; export PATH={}; {script}", q(&self.path)),
            ],
            timeout_seconds: 900,
        })?;
        Ok((
            v["exit_code"].as_i64().ok_or("missing exit code")?,
            v["output"].as_str().ok_or("missing output")?.trim().into(),
        ))
    }
    fn shell(
        &mut self,
        node: &str,
        script: impl AsRef<str>,
        success: bool,
        name: &str,
    ) -> Result<String> {
        let (code, out) = self.exec(node, script.as_ref())?;
        require(
            code >= 0 && (code == 0) == success,
            format!("{name}: exit {code}: {out}"),
        )?;
        self.check(true, name)?;
        Ok(out)
    }
    fn wait(&mut self, node: &str, script: &str, name: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            require(Instant::now() < deadline, format!("deadline: {name}"))?;
            if self.exec(node, script)?.0 == 0 {
                return self.check(true, name);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    fn sql(&mut self, sql: &str, name: &str) -> Result<String> {
        self.shell(
            "primary",
            format!(
                "runuser -u postgres -- psql -Xw -Atq -v ON_ERROR_STOP=1 -d postgres -c {}",
                q(sql)
            ),
            true,
            name,
        )
    }
    fn publish(&mut self, node: &str, path: &str, value: &Value, name: &str) -> Result<()> {
        self.shell(
            node,
            format!(
                "printf '%s\\n' {} > {}; chown root:root {}; chmod 0644 {}",
                q(&serde_json::to_string(value)?),
                q(path),
                q(path),
                q(path)
            ),
            true,
            name,
        )?;
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
        let (ca, a) = self.exec(from, &format!("sha256sum {}", q(source)))?;
        let (cb, b) = self.exec(to, &format!("sha256sum {}", q(destination)))?;
        let digest = a.split_whitespace().next().ok_or("missing digest")?;
        self.check(
            ca == 0 && cb == 0 && digest.len() == 64 && b.split_whitespace().next() == Some(digest),
            name,
        )?;
        std::fs::remove_file(host)?;
        Ok(())
    }
    fn align(&mut self, receiver: &str, sender: &str, name: &str) -> Result<()> {
        let mut seconds = 0_u64;
        for node in [receiver, sender] {
            let (code, out) = self.exec(node, "date +%s")?;
            require(code == 0, "clock read")?;
            seconds = seconds.max(out.parse()?);
        }
        self.shell(receiver, format!("date --set=@{seconds}"), true, name)?;
        Ok(())
    }
}
fn run(args: Args) -> Result<()> {
    let c: Config = serde_json::from_str(&args.config)?;
    for p in c.tool_roots.iter().chain([
        &c.native_package,
        &c.legacy_package,
        &c.postgres_package,
        &c.coreutils_package,
        &c.shell,
    ]) {
        require(
            p.starts_with("/nix/store/")
                && !p.contains("..")
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.+".contains(&b)),
            "invalid store path",
        )?;
    }
    require(
        args.control_fd >= 0 && !c.tool_roots.is_empty(),
        "invalid configuration",
    )?;
    // SAFETY: this one Client takes ownership of the inherited transport descriptor.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    let mut g = Gate {
        client: Client::new(stream, Duration::from_secs(960))?,
        shell: c.shell.clone(),
        path: c
            .tool_roots
            .iter()
            .map(|p| format!("{p}/bin"))
            .collect::<Vec<_>>()
            .join(":"),
        began: Instant::now(),
        assertions: BTreeSet::new(),
    };
    for node in ["primary", "certifier"] {
        let started = g.call(Request::Start {
            node: node.into(),
            allow_reboot: false,
        })?;
        g.check(started["completed"] == true, &format!("{node}-started"))?;
        g.wait(
            node,
            "systemctl is-active multi-user.target",
            &format!("{node}-booted"),
        )?;
    }
    g.wait("primary", "systemctl is-active postgresql.service; test \"$(systemctl show postgresql-setup.service -p Result --value)\" = success", "real-primary-role-setup")?;
    let source_host = g.shell(
        "primary",
        "cat /proc/sys/kernel/hostname",
        true,
        "source-hostname-observed",
    )?;
    let remote_host = g.shell(
        "certifier",
        "cat /proc/sys/kernel/hostname",
        true,
        "certifier-hostname-observed",
    )?;
    let source_machine = g.shell("primary", "cat /etc/machine-id", true, "source-machine-id")?;
    let remote_machine = g.shell(
        "certifier",
        "cat /etc/machine-id",
        true,
        "certifier-machine-id",
    )?;
    g.check(
        source_host == "primary" && remote_host == "certifier" && source_machine != remote_machine,
        "distinct-real-hosts",
    )?;
    g.sql("CREATE ROLE application LOGIN; CREATE TABLE recovery_records(id integer PRIMARY KEY, body text NOT NULL, revision bigint NOT NULL, payload bytea NOT NULL); INSERT INTO recovery_records VALUES (1,'original café',7,decode('00ff1020','hex')),(2,'deleted 雪',1,decode('010203','hex')); CHECKPOINT", "acknowledged-nonempty-prebackup-records")?;
    let identifier = g.sql(
        "SELECT system_identifier::text FROM pg_control_system()",
        "real-control-system-identity",
    )?;
    require(
        identifier.bytes().all(|b| b.is_ascii_digit()) && !identifier.is_empty(),
        "invalid system identity",
    )?;
    let config = json!({"resource":"source-local-fixture","state_dir":"/var/lib/demo-primary-authority","data_dir":"/var/lib/postgresql/18","major":"18","package":c.postgres_package,"required_mounts":[],
        "writer_fence":{"control_role":"postgres","replication_roles":["replicator"],"allowed_preload_libraries":[]},
        "recovery":{"repository_protocol":"source-local-v1","require_writer_fence":true,"system_identifier":identifier,"source_hostname":source_host,"backup_root":ROOT,"snapshot_file":format!("{ROOT}/evidence/snapshot.json"),"receipt_file":format!("{ROOT}/evidence/receipt.json"),"off_host_receipt_file":format!("{ROOT}/evidence/off-host.json"),"max_age_seconds":3600,"verify_timeout_seconds":900,"record_checks":[{"name":"full-records","database":"postgres","sql":RECORDS}]}});
    g.publish(
        "primary",
        CONFIG,
        &config,
        "root-owned-runtime-primary-config",
    )?;
    g.shell("primary", format!("test \"$(stat -c '%U:%a' {CONFIG})\" = root:644; install -d -m 0700 -o postgres -g postgres /var/lib/demo-primary-authority {ROOT}/evidence; systemctl start pg-receivewal.service"), true, "private-authority-and-generated-receiver")?;
    g.wait("primary", "systemctl is-active pg-receivewal.service; test \"$(runuser -u postgres -- psql -Xw -Atqc \"SELECT count(*) FROM pg_stat_replication WHERE usename='replicator' AND client_addr='127.0.0.1'\")\" = 1", "actual-loopback-receiver-streaming")?;
    g.shell("primary", "systemctl start pg-basebackup.service; test \"$(systemctl show pg-basebackup.service -p Result --value)\" = success; test \"$(systemctl show pg-basebackup.service -p ActiveState --value)\" = inactive", true, "generated-basebackup-completed")?;
    let backup = g.shell(
        "primary",
        format!(
            "find {ROOT}/base -mindepth 1 -maxdepth 1 -type d ! -name '*.partial' -printf '%f\\n'"
        ),
        true,
        "actual-complete-backup-inventory",
    )?;
    require(
        !backup.is_empty()
            && !backup.contains('\n')
            && backup
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "expected one safe complete backup",
    )?;
    g.shell(
        "primary",
        format!(
            "runuser -u postgres -- {}/bin/pg_verifybackup {ROOT}/base/{}",
            q(&c.postgres_package),
            q(&backup)
        ),
        true,
        "actual-backup-manifest-verifies",
    )?;
    let marker_script = format!(
        "stat -c '%d:%i' {ROOT}/LAST_SUCCESS; sha256sum {ROOT}/LAST_SUCCESS; cat {ROOT}/LAST_SUCCESS"
    );
    let marker = g.shell(
        "primary",
        &marker_script,
        true,
        "legacy-marker-before-capture",
    )?;
    let iso = marker.lines().last().ok_or("missing LAST_SUCCESS")?;
    g.check(
        iso != backup && iso.contains('T') && iso.contains(':'),
        "last-success-remains-iso-not-backup-id",
    )?;
    g.shell(
        "primary",
        "systemctl stop pg-receivewal.service",
        true,
        "receiver-stopped-before-new-acknowledgements",
    )?;
    g.sql("BEGIN; UPDATE recovery_records SET body='改訂 café',revision=8 WHERE id=1; DELETE FROM recovery_records WHERE id=2; INSERT INTO recovery_records VALUES (3,'new 🐘',2,decode('deadbeef','hex')); COMMIT; CHECKPOINT", "postbackup-insert-update-delete-acknowledged")?;
    let actual = g.sql(RECORDS, "postbackup-full-record-readback")?;
    g.check(actual == EXPECTED, "two-exact-nonempty-final-records")?;
    let pg = format!(
        "runuser -u postgres -- {}/bin/harbor-db-postgres --config {CONFIG}",
        q(&c.native_package)
    );
    g.shell(
        "primary",
        "systemctl stop postgresql.service",
        true,
        "offline-for-explicit-fence-open",
    )?;
    let opened: Value = serde_json::from_str(&g.shell(
        "primary",
        format!("{pg} fence-open --system-identifier {}", q(&identifier)),
        true,
        "native-fence-open",
    )?)?;
    let token = field(&opened, "token")?.to_owned();
    let inspect_fence = format!("{pg} inspect-fence --token {}", q(&token));
    g.shell(
        "primary",
        "systemctl start postgresql.service",
        true,
        "retained-fence-primary-restarted",
    )?;
    g.wait(
        "primary",
        &inspect_fence,
        "native-live-fence-proves-exclusion",
    )?;
    let denied = "runuser -u postgres -- env PGCONNECT_TIMEOUT=5 psql -Xw -h 127.0.0.1 -U application -d postgres -Atqc 'SELECT 1'";
    g.shell("primary", denied, false, "ordinary-tcp-application-denied")?;
    let capture = format!(
        "{pg} capture-backup --backup-id {} --capture-id new --socket-dir /run/postgresql --port 5432",
        q(&backup)
    );
    let failure = g.shell(
        "primary",
        format!("{capture} --wal-wait-seconds 1 2>&1"),
        false,
        "missing-receiver-wal-rejects-finalization",
    )?;
    require(
        failure.contains("missing complete receiver WAL"),
        format!("unexpected finalization diagnostic: {failure}"),
    )?;
    g.check(true, "failure-is-actual-missing-wal")?;
    let pin_script = format!(
        "stat -c '%d:%i' {ROOT}/recovery/pins/new.json; sha256sum {ROOT}/recovery/pins/new.json; cat {ROOT}/recovery/pins/new.json"
    );
    let pin = g.shell(
        "primary",
        &pin_script,
        true,
        "frozen-intent-bytes-and-inode",
    )?;
    g.shell("primary", format!("test ! -e {ROOT}/recovery/SELECTED; test ! -e {ROOT}/recovery/captures/new.json; test ! -e {ROOT}/recovery/snapshots/new.json; systemctl start pg-backup-prune.service; test \"$(systemctl show pg-backup-prune.service -p Result --value)\" = success; test -f {ROOT}/base/{}/backup_manifest; test -f {ROOT}/recovery/pins/new.json", q(&backup)), true, "unfinalized-pin-survives-real-prune-with-no-selection")?;
    g.sql("CHECKPOINT", "background-wal-after-frozen-capture-target")?;
    g.sql(&format!("SELECT pg_current_wal_flush_lsn() > (pg_read_file('{ROOT}/recovery/pins/new.json')::json->>'post_backup_lsn')::pg_lsn"), "later-flush-position-readback").and_then(|out| g.check(out == "t", "later-background-wal-exceeds-frozen-completed-target"))?;
    g.shell(
        "primary",
        "systemctl start pg-receivewal.service",
        true,
        "receiver-restarted-under-live-fence",
    )?;
    g.wait("primary", "systemctl is-active pg-receivewal.service; test \"$(runuser -u postgres -- psql -Xw -Atqc \"SELECT count(*) FROM pg_stat_replication WHERE usename='replicator' AND client_addr='127.0.0.1'\")\" = 1", "fenced-scram-physical-reconnection")?;
    let finalized: Value = serde_json::from_str(&g.shell(
        "primary",
        format!("{capture} --wal-wait-seconds 30"),
        true,
        "same-id-interrupted-finalization-retry",
    )?)?;
    let pin_after = g.shell("primary", &pin_script, true, "retry-pin-readback")?;
    let marker_after = g.shell(
        "primary",
        &marker_script,
        true,
        "retry-legacy-marker-readback",
    )?;
    g.check(
        pin == pin_after && marker == marker_after,
        "retry-preserves-frozen-intent-and-legacy-marker-inodes-and-bytes",
    )?;
    g.shell("primary", format!("cmp {ROOT}/recovery/pins/new.json {ROOT}/recovery/captures/new.json; test \"$(cat {ROOT}/recovery/SELECTED)\" = new"), true, "selected-capture-exactly-matches-pin")?;
    let meta: Value = serde_json::from_str(&g.shell(
        "primary",
        format!("cat {ROOT}/recovery/captures/new.json"),
        true,
        "actual-producer-metadata",
    )?)?;
    let target = field(&meta, "post_backup_lsn")?.to_owned();
    let (hi, lo) = target.split_once('/').ok_or("invalid target LSN")?;
    let target_number = (u64::from_str_radix(hi, 16)? << 32) | u64::from_str_radix(lo, 16)?;
    let size = meta["wal_segment_bytes"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or("missing segment size")?;
    let number = target_number.checked_sub(1).ok_or("zero target")? / size;
    let per_log = (1_u64 << 32) / size;
    let missing = format!(
        "{:08X}{:08X}{:08X}",
        meta["timeline"].as_u64().ok_or("missing timeline")?,
        number / per_log,
        number % per_log
    );
    g.check(
        meta["writer_fence_token"] == token
            && meta["backup_id"] == backup
            && meta["system_identifier"] == identifier
            && finalized["recovery_target_lsn"] == target
            && meta["record_hashes"] == finalized["records"],
        "capture-binds-real-identity-fence-target-and-records",
    )?;
    g.shell("primary", "systemctl stop pg-receivewal.service; tar -C /srv/pgbackup -cf /run/source-local.tar 127.0.0.1", true, "unchanged-repository-archive-created")?;
    let host = args.acceptance.with_extension("transfer.tar");
    g.transfer(
        "primary",
        "/run/source-local.tar",
        "certifier",
        "/run/source-local.tar",
        &host,
        "all-repository-artifacts-transported-byte-identically",
    )?;
    g.shell("certifier", format!("install -d -m 0700 /srv/pgbackup; tar -C /srv/pgbackup -xf /run/source-local.tar; chown -R postgres:postgres /srv/pgbackup; install -d -m 0700 -o postgres -g postgres /srv/recovered /srv/recovery-socket /var/lib/demo-primary-authority; cp -a {ROOT}/base/{} /srv/recovered/18; sha256sum /srv/recovered/18/postgresql.auto.conf > /run/restored-auto.sha256", q(&backup)), true, "private-independent-disposable-restore")?;
    let mut remote_config = config.clone();
    remote_config["recovery"]["require_writer_fence"] = json!(false);
    g.publish(
        "certifier",
        CONFIG,
        &remote_config,
        "root-owned-certifier-reader-config",
    )?;
    let restore = format!(
        "data_directory = '/srv/recovered/18'\nlisten_addresses = ''\nunix_socket_directories = '/srv/recovery-socket'\nport = 55432\nrestore_command = '{}/bin/cp {ROOT}/wal/%f %p'\nrecovery_target_lsn = '{target}'\nrecovery_target_action = 'promote'\ndefault_transaction_read_only = on\nhba_file = '/srv/recovered/private-hba.conf'\nident_file = '/srv/recovered/private-ident.conf'\n",
        c.coreutils_package
    );
    g.shell("certifier", format!("printf '%s' {} > /srv/recovered/18/postgresql.conf; printf 'local all postgres peer\\n' > /srv/recovered/private-hba.conf; touch /srv/recovered/private-ident.conf /srv/recovered/18/recovery.signal; chown postgres:postgres /srv/recovered/18/postgresql.conf /srv/recovered/18/recovery.signal /srv/recovered/private-*.conf; chmod 0600 /srv/recovered/private-*.conf; mv {ROOT}/wal/{missing} /srv/recovered/missing-wal" , q(&restore)), true, "private-restore-config-and-real-necessary-wal-withheld")?;
    let ctl = format!(
        "runuser -u postgres -- {}/bin/pg_ctl -D /srv/recovered/18",
        q(&c.postgres_package)
    );
    // pg_ctl may report ready before replay fails, so assert the server's actual terminal diagnostic.
    g.shell("certifier", format!("{ctl} -p {}/bin/postgres -l /srv/recovered/missing.log -t 30 -w start || true; for i in $(seq 1 60); do if grep -q 'recovery ended before configured recovery target was reached' /srv/recovered/missing.log; then exit 0; fi; sleep 1; done; cat /srv/recovered/missing.log; exit 1", q(&c.postgres_package)), true, "real-replay-cannot-reach-target-with-missing-wal")?;
    g.shell("certifier", format!("{ctl} -m immediate -w stop || true; rm -rf /srv/recovered/18; cp -a {ROOT}/base/{} /srv/recovered/18; mv /srv/recovered/missing-wal {ROOT}/wal/{missing}; printf '%s' {} > /srv/recovered/18/postgresql.conf; touch /srv/recovered/18/recovery.signal; chown postgres:postgres /srv/recovered/18/postgresql.conf /srv/recovered/18/recovery.signal; {ctl} -p {}/bin/postgres -l /srv/recovered/server.log -t 60 -w start", q(&backup), q(&restore), q(&c.postgres_package)), true, "original-wal-restored-and-fresh-disposable-replay-started")?;
    let remote_sql = format!(
        "runuser -u postgres -- {}/bin/psql -Xw -h /srv/recovery-socket -p 55432 -Atq -v ON_ERROR_STOP=1 -d postgres",
        q(&c.postgres_package)
    );
    g.wait(
        "certifier",
        &format!("test \"$({remote_sql} -c 'SELECT NOT pg_is_in_recovery()')\" = t"),
        "independent-recovery-promoted",
    )?;
    let restored = g.shell(
        "certifier",
        format!("{remote_sql} -c {}", q(RECORDS)),
        true,
        "restored-full-record-readback",
    )?;
    g.check(
        restored == EXPECTED,
        "replayed-postbackup-inserts-updates-deletes-and-revisions",
    )?;
    g.shell("certifier", format!("test \"$({remote_sql} -c {})\" = t; sha256sum -c /run/restored-auto.sha256", q(&format!("SELECT pg_last_wal_replay_lsn() >= '{target}'::pg_lsn AND current_setting('default_transaction_read_only')='on'"))), true, "actual-replay-target-read-only-and-auto-config-preserved")?;
    g.align("certifier", "primary", "certifier-clock-aligned")?;
    let receipt: Value = serde_json::from_str(&g.shell("certifier", format!("{pg} certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432"), true, "native-independent-certification")?)?;
    g.check(
        receipt["executor_host"] == remote_host
            && receipt["records"] == finalized["records"]
            && receipt["metadata_sha256"] == finalized["metadata_sha256"]
            && receipt["recovery_target_lsn"] == target
            && receipt["status"] == "ready",
        "receipt-binds-actual-certifier-and-producer-hashes",
    )?;
    g.transfer(
        "certifier",
        &format!("{ROOT}/evidence/receipt.json"),
        "primary",
        "/run/independent-receipt.json",
        &host,
        "independent-receipt-returned-byte-identically",
    )?;
    g.shell("primary", format!("install -m 0600 -o postgres -g postgres /run/independent-receipt.json {ROOT}/evidence/receipt.json; install -m 0600 -o postgres -g postgres /run/independent-receipt.json {ROOT}/evidence/off-host.json; cmp {ROOT}/evidence/receipt.json {ROOT}/evidence/off-host.json"), true, "unchanged-independent-receipt-installed-at-both-policy-paths")?;
    g.align("primary", "certifier", "source-clock-aligned")?;
    let admission = format!("{pg} inspect-recovery --socket-dir /run/postgresql --port 5432");
    let admitted: Value = serde_json::from_str(&g.shell(
        "primary",
        &admission,
        true,
        "native-source-full-recovery-admission",
    )?)?;
    g.check(
        admitted["status"] == "ready" && admitted["off_host"] == remote_host,
        "full-admission-requires-actual-independent-host",
    )?;
    let evidence_script = format!(
        "sha256sum {ROOT}/recovery/snapshots/new.json {ROOT}/evidence/receipt.json {ROOT}/evidence/off-host.json; stat -c '%n:%d:%i' {ROOT}/recovery/snapshots/new.json {ROOT}/recovery/SELECTED {ROOT}/recovery/captures/new.json {ROOT}/recovery/pins/new.json"
    );
    let evidence = g.shell(
        "primary",
        &evidence_script,
        true,
        "accepted-evidence-digests",
    )?;
    g.shell(
        "primary",
        format!("sleep 1; {capture} --wal-wait-seconds 1"),
        true,
        "accepted-capture-explicit-repeat",
    )?;
    let repeated = g.shell(
        "primary",
        &evidence_script,
        true,
        "accepted-capture-repeat-evidence-readback",
    )?;
    g.check(
        evidence == repeated,
        "accepted-repeat-preserves-certified-snapshot-and-all-generation-inodes",
    )?;
    g.shell(
        "primary",
        &admission,
        true,
        "accepted-repeat-does-not-invalidate-receipts",
    )?;
    let python_admission = format!(
        "runuser -u postgres -- {}/bin/harbor-db-postgres --config {CONFIG} inspect-recovery --socket-dir /run/postgresql --port 5432",
        q(&c.legacy_package)
    );
    let python: Value = serde_json::from_str(&g.shell(
        "primary",
        &python_admission,
        true,
        "python-default-consumes-real-native-capture",
    )?)?;
    g.check(
        python == admitted,
        "python-native-source-admission-exact-public-result-equality",
    )?;
    let mut drift = meta.clone();
    drift["record_hashes"]["full-records"] = json!("0".repeat(64));
    g.shell("primary", format!("cp -p {ROOT}/recovery/captures/new.json /run/original-capture; printf '%s\\n' {} > {ROOT}/recovery/captures/new.json", q(&serde_json::to_string(&drift)?)), true, "producer-artifact-copy-mutated-for-negative-case")?;
    g.shell(
        "primary",
        &admission,
        false,
        "metadata-drift-invalidates-accepted-receipts",
    )?;
    let after = g.shell(
        "primary",
        &evidence_script,
        true,
        "rejected-admission-evidence-readback",
    )?;
    g.check(
        evidence == after,
        "rejection-does-not-rewrite-snapshot-or-receipts",
    )?;
    g.shell("primary", format!("cp -p /run/original-capture {ROOT}/recovery/captures/new.json; cmp {ROOT}/recovery/captures/new.json {ROOT}/recovery/pins/new.json; {admission}"), true, "exact-original-metadata-restores-admission-without-reimport")?;
    g.shell(
        "primary",
        &inspect_fence,
        true,
        "gate-ends-with-same-live-fence",
    )?;
    g.shell(
        "primary",
        denied,
        false,
        "gate-end-ordinary-application-still-denied",
    )?;
    let fence: Value = serde_json::from_str(&g.shell(
        "primary",
        "cat /var/lib/demo-primary-authority/writer-fence.json",
        true,
        "retained-fence-marker-readback",
    )?)?;
    g.check(
        fence["token"] == token,
        "gate-never-thaws-or-replaces-fence",
    )?;
    let assertions: Vec<_> = g
        .assertions
        .into_iter()
        .map(|name| json!({"name":name,"passed":true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.acceptance,
        &json!({"schema":1,"case_id":harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-source-local-recovery"),"assertions":assertions}),
    )?;
    Ok(())
}
fn main() -> Result<()> {
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(3600)).is_err() {
            eprintln!("source-local recovery overall 3600-second deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
