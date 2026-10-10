//! Generated backup-service acceptance; this is not recovery-ready evidence.
use clap::Parser;
use harbor_db::testing::protocol::{Client, Request};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const ROOT: &str = "/srv/pgbackup/192.168.1.10";
// Inactive manual units may be unloaded by systemd until their first start.
const RESET_FAILED: &str = "for unit in pg-basebackup.service pg-backup-prune.service; do if test \"$(systemctl is-failed \"$unit\")\" = failed; then systemctl reset-failed \"$unit\"; fi; done";
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
struct Gate {
    client: Client,
    shell: String,
    path: String,
    assertions: BTreeSet<String>,
}
impl Gate {
    fn check(&mut self, ok: bool, name: &str) -> Result<()> {
        require(ok, name)?;
        require(self.assertions.insert(name.into()), "duplicate assertion")
    }
    fn exec(&mut self, node: &str, script: &str) -> Result<(i64, String)> {
        let v = self.client.call(Request::Execute {
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
        let (code, output) = self.exec(node, script.as_ref())?;
        require(
            code >= 0 && (code == 0) == success,
            format!("{name}: exit {code}: {output}"),
        )?;
        self.check(true, name)?;
        Ok(output)
    }
    fn wait(&mut self, node: &str, script: &str, name: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            require(Instant::now() < deadline, format!("deadline: {name}"))?;
            if self.exec(node, script)?.0 == 0 {
                return self.check(true, name);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn sql(&mut self, sql: &str, name: &str) -> Result<String> {
        self.shell(
            "primary",
            format!(
                "runuser -u postgres -- psql -Xw -At -v ON_ERROR_STOP=1 -d postgres -c {}",
                q(sql)
            ),
            true,
            name,
        )
    }
    fn start_backup(&mut self, tag: &str) -> Result<()> {
        self.shell(
            "backup",
            format!("{RESET_FAILED}; systemctl start --no-block pg-basebackup.service"),
            true,
            &format!("{tag}-started"),
        )?;
        self.wait("primary", "test \"$(runuser -u postgres -- psql -Xw -At -d postgres -c 'SELECT count(*) FROM pg_stat_progress_basebackup WHERE backup_streamed > 0')\" = 1", &format!("{tag}-real-postgres-progress"))?;
        self.wait("backup", &format!("pid=$(systemctl show pg-basebackup.service -p MainPID --value); test \"$pid\" -gt 0; test -n \"$(find {ROOT}/base -path \"*-$pid.partial/*\" -type f -size +0c -print -quit)\""), &format!("{tag}-nonempty-partial"))
    }
    fn interrupt(&mut self, tag: &str) -> Result<()> {
        self.shell(
            "backup",
            "systemctl kill --kill-who=all --signal=SIGKILL pg-basebackup.service",
            true,
            &format!("{tag}-cgroup-killed"),
        )?;
        self.wait("backup", "test \"$(systemctl show pg-basebackup.service -p ActiveState --value)\" = failed; test \"$(systemctl show pg-basebackup.service -p Result --value)\" = signal", &format!("{tag}-unit-failure-recorded"))?;
        self.wait("primary", "test \"$(runuser -u postgres -- psql -Xw -At -d postgres -c 'SELECT count(*) FROM pg_stat_progress_basebackup')\" = 0", &format!("{tag}-source-transfer-ended"))
    }
    fn complete(&mut self, tag: &str) -> Result<Vec<String>> {
        self.shell("backup", format!("{RESET_FAILED}; systemctl start pg-basebackup.service; test \"$(systemctl show pg-basebackup.service -p ActiveState --value)\" = inactive; test \"$(systemctl show pg-basebackup.service -p Result --value)\" = success"), true, &format!("{tag}-oneshot-success"))?;
        let trees = self.trees()?;
        for (i, tree) in trees.iter().enumerate() {
            self.shell(
                "backup",
                format!("runuser -u postgres -- pg_verifybackup {}", q(tree)),
                true,
                &format!("{tag}-real-manifest-verified-{i}"),
            )?;
        }
        Ok(trees)
    }
    fn trees(&mut self) -> Result<Vec<String>> {
        let (code, out) = self.exec(
            "backup",
            &format!("find {ROOT}/base -mindepth 1 -maxdepth 1 -type d ! -name '*.partial' | sort"),
        )?;
        require(code == 0, "complete inventory failed")?;
        Ok(out.lines().map(str::to_owned).collect())
    }
    fn snapshot(&mut self, tree: &str) -> Result<String> {
        let (code, out) = self.exec("backup", &format!("stat -c '%d:%i' {ROOT}/BACKUP_LOCK {ROOT}/LAST_SUCCESS; cat {ROOT}/LAST_SUCCESS; stat -c '%a:%U:%G' {ROOT} {ROOT}/base {ROOT}/wal; find {} -printf '%P %y %s %m %U %G %D %i %T@\\n' | sort; find {} -type f -print0 | sort -z | xargs -0 sha256sum", q(tree), q(tree)))?;
        require(code == 0, "publication snapshot failed")?;
        Ok(out)
    }
}
fn run(args: Args) -> Result<()> {
    let c: Config = serde_json::from_str(&args.config)?;
    for p in c.tool_roots.iter().chain([
        &c.native_package,
        &c.legacy_package,
        &c.postgres_package,
        &c.shell,
    ]) {
        require(
            p.starts_with("/nix/store/")
                && !p.contains("..")
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.+".contains(&b)),
            "invalid immutable package path",
        )?;
    }
    require(
        args.control_fd >= 0 && !c.tool_roots.is_empty(),
        "invalid configuration",
    )?;
    // SAFETY: the inherited socket is transferred to this single Client.
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
        assertions: BTreeSet::new(),
    };
    for node in ["primary", "backup"] {
        let v = g.client.call(Request::Start {
            node: node.into(),
            allow_reboot: false,
        })?;
        g.check(v["completed"] == true, &format!("{node}-started"))?;
        g.wait(
            node,
            "systemctl is-active multi-user.target",
            &format!("{node}-booted"),
        )?;
    }
    g.wait("primary", "test \"$(systemctl is-active postgresql.service)\" = active; test \"$(systemctl show postgresql-setup.service -p Result --value)\" = success; test \"$(runuser -u postgres -- psql -Xw -At -d postgres -c \"SELECT count(*) FROM pg_roles WHERE rolname='replicator' AND rolreplication AND rolcanlogin\")\" = 1", "source-role-setup-complete")?;
    let (code, password) = g.exec("primary", "cat /run/backup-credential/password")?;
    require(
        code == 0 && password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit()),
        "runtime credential invalid",
    )?;
    g.shell("backup", format!("install -d -m 0700 -o postgres -g postgres /run/backup-credential; printf '%s' {} > /run/backup-credential/password; chown postgres:postgres /run/backup-credential/password; chmod 0400 /run/backup-credential/password", q(&password)), true, "target-private-runtime-credential")?;
    for (unit, expected) in [
        (
            "pg-basebackup",
            vec!["harbor-db-durable", "publish-tree", "pg_verifybackup"],
        ),
        ("pg-backup-prune", vec!["harbor-db-backup-prune"]),
    ] {
        let script = g.shell("backup", format!("unit_script=$(systemctl show {unit}.service -p ExecStart --value | grep -o '/nix/store/[^ ;}}]*' | head -n 1); test -f \"$unit_script\"; cat \"$unit_script\"; systemctl show {unit}.service -p User -p ProtectSystem -p UMask"), true, &format!("{unit}-effective-execstart-read"))?;
        g.check(
            expected.iter().all(|name| script.contains(name))
                && script.contains(&c.native_package)
                && script.contains("User=postgres")
                && script.contains("ProtectSystem=strict")
                && script.contains("UMask=0077"),
            &format!("{unit}-native-private-unit"),
        )?;
    }
    g.shell("backup", "test \"$(systemctl is-enabled pg-basebackup.timer 2>/dev/null || true)\" != enabled; systemctl start pg-receivewal.service", true, "manual-schedule-and-receiver-start")?;
    g.wait(
        "backup",
        "systemctl is-active pg-receivewal.service",
        "real-receiver-active",
    )?;
    g.sql("CREATE TABLE backup_records(id integer PRIMARY KEY, body text NOT NULL, revision integer NOT NULL, payload text NOT NULL); ALTER TABLE backup_records ALTER COLUMN payload SET STORAGE EXTERNAL; INSERT INTO backup_records VALUES (1, 'acknowledged generated-service record', 7, repeat(md5('service-record'), 262144)); CHECKPOINT", "nonempty-revision-seven-record-acknowledged")?;
    let row = g.sql(
        "SELECT id || ':' || revision || ':' || octet_length(payload) FROM backup_records",
        "source-record-readback",
    )?;
    g.check(
        row == "1:7:8388608",
        "source-acknowledged-eight-mib-payload",
    )?;
    g.sql("SELECT pg_switch_wal()", "real-wal-switch")?;
    g.wait("backup", &format!("test -n \"$(find {ROOT}/wal -regextype posix-extended -regex '.*/[0-9A-F]{{24}}' -type f -size 16777216c -print -quit)\""), "nonempty-complete-received-wal-segment")?;
    g.start_backup("initial")?;
    g.interrupt("initial")?;
    g.shell("backup", format!("test ! -e {ROOT}/LAST_SUCCESS; test -z \"$(find {ROOT}/base -mindepth 1 -maxdepth 1 -type d ! -name '*.partial' -print -quit)\""), true, "initial-failure-no-success-publication")?;
    let first = g.complete("first-retry")?;
    g.check(first.len() == 1, "first-retry-one-complete-tree")?;
    let prior = g.snapshot(&first[0])?;
    let marker = g.shell(
        "backup",
        format!("cat {ROOT}/LAST_SUCCESS"),
        true,
        "legacy-marker-read",
    )?;
    g.shell("backup", format!("printf '%s\\n' {} | grep -Eq '^[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}T[0-9]{{2}}:[0-9]{{2}}:[0-9]{{2}}[+-][0-9]{{2}}:[0-9]{{2}}$'; date --date={} +%s >/dev/null; test -z \"$(find {ROOT}/base -name '*.meta.json' -print -quit)\"", q(&marker), q(&marker)), true, "legacy-timestamp-contract-without-invented-metadata")?;
    let parity = format!(
        "cat {ROOT}/LAST_SUCCESS | {}/bin/harbor-db-durable write /srv/pgbackup/marker-parity/python-marker; cmp {ROOT}/LAST_SUCCESS /srv/pgbackup/marker-parity/python-marker; cat /srv/pgbackup/marker-parity/python-marker | {}/bin/harbor-db-durable write /srv/pgbackup/marker-parity/native-marker; cmp {ROOT}/LAST_SUCCESS /srv/pgbackup/marker-parity/native-marker",
        q(&c.legacy_package),
        q(&c.native_package),
    );
    g.shell("backup", format!("install -d -m 0700 -o postgres -g postgres /srv/pgbackup/marker-parity; runuser -u postgres -- {} -c {}", q(&c.shell), q(&format!("set -euo pipefail; export PATH={}; {parity}", q(&g.path)))), true, "service-marker-bidirectional-helper-byte-parity")?;
    let lock = g.shell(
        "backup",
        format!("stat -c '%d:%i' {ROOT}/BACKUP_LOCK"),
        true,
        "durable-lock-identity-read",
    )?;
    g.sql("INSERT INTO backup_records VALUES (2, 'acknowledged before interrupted second transfer', 7, repeat(md5('second-record'), 262144)); CHECKPOINT", "second-record-acknowledged")?;
    g.start_backup("second")?;
    g.shell(
        "backup",
        "systemctl start pg-backup-prune.service",
        false,
        "separate-prune-lock-contention-refused",
    )?;
    g.shell("backup", "test \"$(systemctl show pg-backup-prune.service -p ActiveState --value)\" = failed; test \"$(systemctl show pg-backup-prune.service -p Result --value)\" = exit-code; journalctl -u pg-backup-prune.service --no-pager | grep -F 'Resource temporarily unavailable'", true, "contending-prune-unit-lock-failure")?;
    let during = g.snapshot(&first[0])?;
    g.check(
        during == prior,
        "live-contention-preserves-marker-inode-bytes-tree-and-lock",
    )?;
    g.interrupt("second")?;
    let after = g.snapshot(&first[0])?;
    g.check(
        after == prior,
        "interruption-preserves-marker-inode-bytes-full-tree-inventory-and-lock",
    )?;
    let interrupted_trees = g.trees()?;
    g.check(
        interrupted_trees == first,
        "second-interruption-no-complete-publication",
    )?;
    let second = g.complete("second-retry")?;
    g.check(
        second.len() == 2 && second.contains(&first[0]),
        "second-retry-retains-first-and-publishes-second",
    )?;
    let new_marker = g.shell(
        "backup",
        format!("cat {ROOT}/LAST_SUCCESS"),
        true,
        "retry-marker-read",
    )?;
    g.check(new_marker != marker, "successful-retry-distinct-iso-marker")?;
    let third = g.complete("third-transfer")?;
    g.check(third.len() == 3, "three-real-verified-complete-trees")?;
    let partials = g.shell("backup", format!("find {ROOT}/base -mindepth 1 -maxdepth 1 -name '*.partial' -printf '%f %D %i\\n' | sort"), true, "interrupted-partials-inventory")?;
    g.check(
        partials.lines().count() == 2,
        "two-interrupted-partials-retained",
    )?;
    g.shell("backup", format!("touch -m -d '3 days ago' {}; {RESET_FAILED}; systemctl start pg-backup-prune.service; test \"$(systemctl show pg-backup-prune.service -p Result --value)\" = success", q(&first[0])), true, "actual-prune-expires-aged-oldest")?;
    let retained = g.trees()?;
    g.check(
        retained.len() == 2
            && !retained.contains(&first[0])
            && retained.iter().all(|p| third.contains(p)),
        "prune-retains-latest-two-complete-trees",
    )?;
    let (code, remaining) = g.exec("backup", &format!("find {ROOT}/base -mindepth 1 -maxdepth 1 -name '*.partial' -printf '%f %D %i\\n' | sort; stat -c '%d:%i' {ROOT}/BACKUP_LOCK"))?;
    g.check(
        code == 0 && remaining == format!("{partials}\n{lock}"),
        "prune-preserves-partials-and-durable-lock-inode",
    )?;
    let assertions: Vec<_> = g
        .assertions
        .into_iter()
        .map(|name| json!({"name": name, "passed": true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.acceptance,
        &json!({"schema": 1, "case_id": harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-postgres-backup-service"), "assertions": assertions}),
    )?;
    Ok(())
}
fn main() -> Result<()> {
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(3600)).is_err() {
            eprintln!("backup-service overall 3600-second deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
