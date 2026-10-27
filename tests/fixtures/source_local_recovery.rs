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
    identity_preload: String,
    postgres_package: String,
    coreutils_package: String,
    shell: String,
    tool_roots: Vec<String>,
    replication_roles: Vec<String>,
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
        "writer_fence":{"control_role":"postgres","replication_roles":c.replication_roles,"allowed_preload_libraries":[]},
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
    let selected_before = g.shell(
        "primary",
        format!("stat -c '%d:%i' {ROOT}/recovery/SELECTED; sha256sum {ROOT}/recovery/SELECTED"),
        true,
        "fresh-selection-before-slow-capture",
    )?;
    let slow_config = "/run/slow-capture.json";
    let mut slow: Value = serde_json::from_str(&g.shell(
        "primary",
        format!("cat {CONFIG}"),
        true,
        "read-slow-capture-source-policy",
    )?)?;
    slow["recovery"]["max_age_seconds"] = json!(2);
    // Record checks are one PostgreSQL-prepared statement. Delay once while
    // retaining the exact ordered record columns used by the normal capture.
    slow["recovery"]["record_checks"][0]["sql"] = json!(format!(
        "SELECT records.* FROM pg_sleep(4), ({}) AS records ORDER BY records.id",
        field(&slow["recovery"]["record_checks"][0], "sql")?
    ));
    g.shell("primary", format!("printf '%s\\n' {} > {slow_config}; chown postgres:postgres {slow_config}; touch {ROOT}/base/{}/backup_manifest", q(&slow.to_string()), q(&backup)), true, "bounded-slow-record-capture-config")?;
    let stale = g.shell("primary", format!("runuser -u postgres -- {}/bin/harbor-db-postgres --config {slow_config} capture-backup --backup-id {} --capture-id stale-slow --socket-dir /run/postgresql --port 5432 --wal-wait-seconds 30 2>&1", q(&c.native_package), q(&backup)), false, "slow-capture-expiry-rejects-selection")?;
    g.check(
        stale.contains("stale"),
        "slow-capture-fails-on-freshness-not-an-unrelated-error",
    )?;
    let selected_after = g.shell(
        "primary",
        format!("stat -c '%d:%i' {ROOT}/recovery/SELECTED; sha256sum {ROOT}/recovery/SELECTED"),
        true,
        "fresh-selection-after-slow-capture",
    )?;
    g.check(
        selected_before == selected_after,
        "stale-capture-preserves-selected-inode-and-bytes",
    )?;
    g.shell(
        "primary",
        format!("test -f {ROOT}/recovery/pins/stale-slow.json"),
        true,
        "stale-capture-retains-frozen-attempt",
    )?;
    let meta: Value = serde_json::from_str(&g.shell(
        "primary",
        format!("cat {ROOT}/recovery/captures/new.json"),
        true,
        "actual-producer-metadata",
    )?)?;
    let target = field(&meta, "post_backup_lsn")?.to_owned();
    let source_machine = g
        .shell(
            "primary",
            "sha256sum /etc/machine-id | cut -d' ' -f1",
            true,
            "source-machine-identity-digest",
        )?
        .trim()
        .to_owned();
    let remote_machine = g
        .shell(
            "certifier",
            "sha256sum /etc/machine-id | cut -d' ' -f1",
            true,
            "independent-machine-identity-digest",
        )?
        .trim()
        .to_owned();
    g.check(
        meta["source_machine_sha256"] == source_machine && source_machine != remote_machine,
        "capture-pins-source-machine-and-independent-host-is-distinct",
    )?;
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
    // These are actual alternate mount paths, not symlinks. Certification must
    // reject them before querying an endpoint or publishing a receipt.
    for (name, protected) in [
        ("primary", field(&config, "data_dir")?.to_owned()),
        ("selected-backup", format!("{ROOT}/base/{backup}")),
    ] {
        g.shell("primary",format!("install -d -m 0700 -o postgres -g postgres /srv/restore-alias; mount --bind {} /srv/restore-alias",q(&protected)),true,&format!("{name}-bind-mount-alias-created"))?;
        let rejected=g.shell("primary",format!("{pg} certify-recovery --data-dir /srv/restore-alias --socket-dir /run/postgresql --port 5432 2>&1"),false,&format!("{name}-bind-mount-certification-rejected"))?;
        require(
            rejected.contains("shared storage"),
            format!("{name}-rejection-is-inode-alias-check: {rejected}"),
        )?;
        g.check(true, &format!("{name}-rejection-is-inode-alias-check"))?;
        g.shell(
            "primary",
            "umount /srv/restore-alias; rmdir /srv/restore-alias",
            true,
            &format!("{name}-alias-unmounted-without-starting-postgres"),
        )?;
    }
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
    g.shell("certifier", format!("{ctl} -m immediate -w stop || true; rm -rf /srv/recovered/18; cp -a {ROOT}/base/{} /srv/recovered/18; mv /srv/recovered/missing-wal {ROOT}/wal/{missing}; printf '%s' {} > /srv/recovered/18/postgresql.conf; touch /srv/recovered/18/recovery.signal; chown postgres:postgres /srv/recovered/18/postgresql.conf /srv/recovered/18/recovery.signal; if ! {ctl} -p {}/bin/postgres -l /srv/recovered/server.log -t 60 -w start; then cat /srv/recovered/server.log; exit 1; fi", q(&backup), q(&restore), q(&c.postgres_package)), true, "original-wal-restored-and-fresh-disposable-replay-started")?;
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
    let writable_restore = restore
        .replace("/srv/recovered", "/srv/writable-recovered")
        .replace("/srv/recovery-socket", "/srv/writable-recovery-socket")
        .replace("port = 55432", "port = 55433")
        .replace(
            "default_transaction_read_only = on",
            "default_transaction_read_only = off",
        );
    g.shell("certifier", format!("install -d -m 0700 -o postgres -g postgres /srv/writable-recovered /srv/writable-recovery-socket; cp -a {ROOT}/base/{} /srv/writable-recovered/18; cp -p /srv/recovered/private-*.conf /srv/writable-recovered/; printf '%s' {} > /srv/writable-recovered/18/postgresql.conf; touch /srv/writable-recovered/18/recovery.signal; chown postgres:postgres /srv/writable-recovered/18/postgresql.conf /srv/writable-recovered/18/recovery.signal; runuser -u postgres -- {}/bin/pg_ctl -D /srv/writable-recovered/18 -l /srv/writable-recovered/server.log -t 60 -w start", q(&backup), q(&writable_restore), q(&c.postgres_package)), true, "disposable-writable-negative-replay-started")?;
    g.wait("certifier", &format!("test \"$(runuser -u postgres -- {}/bin/psql -Xw -h /srv/writable-recovery-socket -p 55433 -Atq -d postgres -c {})\" = t", q(&c.postgres_package), q(&format!("SELECT NOT pg_is_in_recovery() AND pg_last_wal_replay_lsn() >= '{target}'::pg_lsn AND current_setting('default_transaction_read_only')='off'"))), "disposable-writable-negative-replay-complete")?;
    for (engine, package) in [("native", &c.native_package), ("python", &c.legacy_package)] {
        let rejected = g.shell("certifier", format!("runuser -u postgres -- {}/bin/harbor-db-postgres --config {CONFIG} certify-recovery --data-dir /srv/writable-recovered/18 --socket-dir /srv/writable-recovery-socket --port 55433 2>&1", q(package)), false, &format!("{engine}-writable-disposable-recovery-rejected"))?;
        g.check(
            rejected.contains("not the declared disposable read-only recovery"),
            &format!("{engine}-unmodified-endpoint-default-observed"),
        )?;
        g.shell(
            "certifier",
            format!(
                "test ! -e {ROOT}/evidence/receipt.json; test ! -e {ROOT}/evidence/off-host.json"
            ),
            true,
            &format!("{engine}-writable-rejection-publishes-no-receipt"),
        )?;
    }
    g.shell(
        "certifier",
        format!(
            "runuser -u postgres -- {}/bin/pg_ctl -D /srv/writable-recovered/18 -w stop",
            q(&c.postgres_package)
        ),
        true,
        "disposable-writable-negative-endpoint-stopped",
    )?;
    let receipt: Value = serde_json::from_str(&g.shell("certifier", format!("{pg} certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432"), true, "native-independent-certification")?)?;
    g.check(
        receipt["executor_host"] == remote_host
            && receipt["executor_machine_sha256"] == remote_machine
            && receipt["source_machine_sha256"] == source_machine
            && receipt["records"] == finalized["records"]
            && receipt["metadata_sha256"] == finalized["metadata_sha256"]
            && receipt["wal_inventory_sha256"] == finalized["wal_inventory_sha256"]
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
    g.shell("primary", "chown root:root /run/independent-receipt.json; chmod 0600 /run/independent-receipt.json; install -d -m 0700 /run/trusted-receipt-import; install -d -m 0777 /run/untrusted-receipt-import; cp /run/independent-receipt.json /run/trusted-receipt-import/worker.json; chown postgres:postgres /run/trusted-receipt-import/worker.json; ln -s /run/independent-receipt.json /run/untrusted-receipt-import/redirect; sha256sum /run/independent-receipt.json > /run/receipt-import-source.sha256", true, "actual-independent-import-source-and-attacks-prepared")?;
    for (engine, package) in [("native", &c.native_package), ("python", &c.legacy_package)] {
        let stage = format!(
            "{}/bin/harbor-db-postgres --config {CONFIG} stage-off-host-receipt",
            q(package)
        );
        for (attack, source) in [
            ("worker-owner", "/run/trusted-receipt-import/worker.json"),
            (
                "writable-ancestor-alias",
                "/run/untrusted-receipt-import/redirect",
            ),
        ] {
            g.shell(
                "primary",
                format!(
                    "{stage} --source {source} --destination /run/trusted-receipt-import/rejected"
                ),
                false,
                &format!("{engine}-{attack}-independent-import-source-rejected"),
            )?;
            g.shell("primary", "test ! -e /run/trusted-receipt-import/rejected; sha256sum -c /run/receipt-import-source.sha256", true, &format!("{engine}-{attack}-rejection-preserves-root-source-and-no-staging"))?;
        }
        g.shell("primary", format!("{stage} --source /run/independent-receipt.json --destination /run/trusted-receipt-import/{engine}; cmp /run/independent-receipt.json /run/trusted-receipt-import/{engine}; test \"$(stat -c '%u:%g:%a' /run/trusted-receipt-import/{engine})\" = 0:0:400"), true, &format!("{engine}-trusted-independent-receipt-pinned-into-private-root-storage"))?;
        g.shell("primary", format!("runuser -u postgres -- {stage} --source /run/independent-receipt.json --destination /run/trusted-receipt-import/worker"), false, &format!("{engine}-unprivileged-import-staging-rejected"))?;
    }
    g.shell("primary", format!("install -m 0600 -o postgres -g postgres /run/independent-receipt.json {ROOT}/evidence/receipt.json; install -m 0600 -o postgres -g postgres /run/independent-receipt.json {ROOT}/evidence/off-host.json; cmp {ROOT}/evidence/receipt.json {ROOT}/evidence/off-host.json"), true, "same-independent-receipt-at-both-paths-installed")?;
    g.align("primary", "certifier", "source-clock-aligned")?;
    let admission = format!("{pg} inspect-recovery --socket-dir /run/postgresql --port 5432");
    // With an off-host executor configured its certification is the single
    // restore proof; the local receipt path is not consulted.
    g.shell(
        "primary",
        &admission,
        true,
        "one-independent-restore-satisfies-executor-policy",
    )?;
    g.shell("primary", format!("install -d -m 0700 -o postgres -g postgres /srv/recovered /srv/recovery-socket; cp -a {ROOT}/base/{} /srv/recovered/18; sha256sum /srv/recovered/18/postgresql.auto.conf > /run/local-restored-auto.sha256; printf '%s' {} > /srv/recovered/18/postgresql.conf; printf 'local all postgres peer\\n' > /srv/recovered/private-hba.conf; touch /srv/recovered/private-ident.conf /srv/recovered/18/recovery.signal; chown postgres:postgres /srv/recovered/18/postgresql.conf /srv/recovered/18/recovery.signal /srv/recovered/private-*.conf; chmod 0600 /srv/recovered/private-*.conf; if ! {ctl} -p {}/bin/postgres -l /srv/recovered/local.log -t 60 -w start; then cat /srv/recovered/local.log; exit 1; fi",q(&backup),q(&restore),q(&c.postgres_package)), true, "source-host-fresh-disposable-physical-replay-started")?;
    g.wait(
        "primary",
        &format!("test \"$({remote_sql} -c 'SELECT NOT pg_is_in_recovery()')\" = t"),
        "source-host-disposable-recovery-promoted",
    )?;
    let local_records = g.shell(
        "primary",
        format!("{remote_sql} -c {}", q(RECORDS)),
        true,
        "source-host-restored-full-record-readback",
    )?;
    g.check(
        local_records == EXPECTED,
        "source-host-replay-preserves-postbackup-changes-and-revisions",
    )?;
    g.shell("primary",format!("test \"$({remote_sql} -c {})\" = t; sha256sum -c /run/local-restored-auto.sha256",q(&format!("SELECT pg_last_wal_replay_lsn() >= '{target}'::pg_lsn AND current_setting('default_transaction_read_only')='on'"))),true,"source-host-replay-target-read-only-and-auto-config-preserved")?;
    let mut namespace_config = config.clone();
    namespace_config["recovery"]["receipt_file"] =
        config["recovery"]["off_host_receipt_file"].clone();
    g.publish(
        "primary",
        "/run/namespace-certifier.json",
        &namespace_config,
        "source-UTS-negative-certifier-policy",
    )?;
    for (engine, package) in [("native", &c.native_package), ("python", &c.legacy_package)] {
        let command = format!(
            "hostname hostname-only-remote; runuser -u postgres -- {}/bin/harbor-db-postgres --config /run/namespace-certifier.json certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432",
            q(package)
        );
        let rejected = g.shell(
            "primary",
            format!("unshare --uts -- sh -ec {} 2>&1", q(&command)),
            false,
            &format!("{engine}-UTS-rename-cannot-certify-independent-recovery"),
        )?;
        g.check(
            rejected.contains("independent machine"),
            &format!("{engine}-UTS-rejection-binds-stable-machine-not-hostname"),
        )?;
    }
    g.shell(
        "primary",
        "printf 'caller-chosen-machine-id\\n' > /run/namespace-fake-machine-id; printf '0 0 4294967295\\n' > /run/namespace-fake-uid-map; chmod 0644 /run/namespace-fake-machine-id /run/namespace-fake-uid-map",
        true,
        "namespace-spoof-inputs-do-not-change-installed-machine-id",
    )?;
    let preload = format!(
        "LD_PRELOAD={} HARBOR_FAKE_MACHINE_ID=/run/namespace-fake-machine-id",
        q(&c.identity_preload)
    );
    g.shell(
        "primary",
        format!("runuser -u postgres -- env {preload} python3 -c {}", q("import socket; assert socket.gethostname()=='loader-chosen-remote'; assert open('/etc/machine-id').read().strip()=='caller-chosen-machine-id'")),
        true,
        "loader-interposer-really-substitutes-hostname-and-machine-id",
    )?;
    let rejected = g.shell(
        "primary",
        format!("runuser -u postgres -- env {preload} {}/bin/harbor-db-postgres --config /run/namespace-certifier.json certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432 2>&1", q(&c.native_package)),
        false,
        "native-off-host-certifier-rejects-loader-controlled-identity",
    )?;
    g.check(
        rejected.contains("independent machine"),
        "static-certifier-retains-same-machine-custody-under-preload",
    )?;
    for (engine, package) in [("native", &c.native_package), ("python", &c.legacy_package)] {
        for fresh_proc in [false, true] {
            let command = format!(
                "mount --bind /run/namespace-fake-machine-id /etc/machine-id; mount --bind /run/namespace-fake-uid-map /proc/self/uid_map; hostname caller-chosen-remote; exec {}/bin/harbor-db-postgres --config /run/namespace-certifier.json certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432",
                q(package)
            );
            let rejected = g.shell(
                "primary",
                format!(
                    "runuser -u postgres -- unshare --user --map-root-user --mount --uts {} -- sh -ec {} 2>&1",
                    if fresh_proc { "--pid --fork --mount-proc" } else { "" },
                    q(&command)
                ),
                false,
                &format!("{engine}-namespace-machine-spoof-fresh-proc-{fresh_proc}-rejected"),
            )?;
            g.check(
                rejected.contains("caller-controlled namespaces"),
                &format!(
                    "{engine}-kernel-pidfd-identity-rejects-mount-and-proc-spoof-{fresh_proc}"
                ),
            )?;
        }
    }
    let local_receipt:Value = serde_json::from_str(&g.shell("primary",format!("{pg} certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432"),true,"native-source-host-disposable-restoration-certification")?)?;
    g.check(
        local_receipt["executor_host"] == source_host
            && local_receipt["executor_machine_sha256"] == source_machine
            && local_receipt["source_machine_sha256"] == source_machine
            && local_receipt["records"] == receipt["records"]
            && local_receipt["metadata_sha256"] == receipt["metadata_sha256"]
            && local_receipt["wal_inventory_sha256"] == receipt["wal_inventory_sha256"]
            && local_receipt["snapshot_sha256"] == receipt["snapshot_sha256"]
            && local_receipt["status"] == "ready",
        "two-distinct-executors-certify-the-same-immutable-physical-capture",
    )?;
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
        "sha256sum {ROOT}/recovery/snapshots/new.json {ROOT}/recovery/captures/new.wal-inventory {ROOT}/evidence/receipt.json {ROOT}/evidence/off-host.json; stat -c '%n:%d:%i' {ROOT}/recovery/snapshots/new.json {ROOT}/recovery/SELECTED {ROOT}/recovery/captures/new.json {ROOT}/recovery/captures/new.wal-inventory {ROOT}/recovery/pins/new.json"
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
    let corrupt = format!(
        "cp -p {ROOT}/wal/{missing} /run/original-wal; printf '\\377' | dd of={ROOT}/wal/{missing} bs=1 seek=0 conv=notrunc status=none; test \"$(stat -c %s {ROOT}/wal/{missing})\" = {size}"
    );
    g.shell(
        "primary",
        corrupt,
        true,
        "required-wal-corrupted-without-changing-carrier-length",
    )?;
    g.shell(
        "primary",
        &admission,
        false,
        "native-rejects-same-length-wal-corruption-after-both-certifications",
    )?;
    g.shell(
        "primary",
        &python_admission,
        false,
        "python-rejects-same-length-wal-corruption-after-both-certifications",
    )?;
    g.shell(
        "primary",
        format!("{capture} --wal-wait-seconds 1"),
        false,
        "accepted-capture-repeat-cannot-rebind-corrupted-wal",
    )?;
    let corruption_evidence = g.shell(
        "primary",
        &evidence_script,
        true,
        "wal-corruption-rejection-evidence-readback",
    )?;
    g.check(
        evidence == corruption_evidence,
        "wal-corruption-rejection-preserves-certification-and-generation-inodes",
    )?;
    g.shell(
        "primary",
        format!("cp -p /run/original-wal {ROOT}/wal/{missing}; {admission}; {python_admission}"),
        true,
        "exact-original-wal-restores-admission-without-recertification",
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
