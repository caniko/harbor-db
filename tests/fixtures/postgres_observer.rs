//! Verify peer-only access and observe a real PostgreSQL session inside COMMIT.
use clap::Parser;
use harbor_db::storage::{Result, durable, invalid};
use harbor_db::testing::protocol::{Client, Request};
use serde_json::{Value, json};
use std::{
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

const DIR: &str = "/run/observer-fixture";
const OBSERVER: &str = "harbor-db-observer";
const ROLE: &str = "harbor_db_observer";
const SENTINEL: &str = "fixture-private-sql-text";

#[derive(Parser)]
struct Args {
    #[arg(long)]
    shell: String,
    #[arg(long)]
    psql: String,
    #[arg(long)]
    cat: String,
    #[arg(long)]
    sleep: String,
    #[arg(long)]
    control_fd: i32,
    #[arg(long)]
    acceptance: PathBuf,
}

fn q(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

struct Gate<'a> {
    args: &'a Args,
    client: Client,
    passed: Vec<Value>,
}

impl Gate<'_> {
    fn check(&mut self, name: &str, passes: bool) -> Result<()> {
        if !passes {
            return Err(invalid(name));
        }
        self.passed.push(json!({"name": name, "passed": true}));
        Ok(())
    }

    fn exec(&mut self, script: &str) -> Result<(i64, String)> {
        let result = self.client.call(Request::execute(
            "machine",
            vec![
                self.args.shell.clone(),
                "-c".into(),
                format!("set -euo pipefail; {script}"),
            ],
        ))?;
        Ok((
            result["exit_code"]
                .as_i64()
                .ok_or_else(|| invalid("bridge exit code missing"))?,
            result["output"]
                .as_str()
                .ok_or_else(|| invalid("bridge output missing"))?
                .trim()
                .into(),
        ))
    }

    fn run(&mut self, script: &str) -> Result<String> {
        let (code, output) = self.exec(script)?;
        if code != 0 {
            return Err(invalid(format!("VM command exit {code}: {output}")));
        }
        Ok(output)
    }

    fn psql(&self, os_user: &str) -> String {
        format!(
            "runuser -u {os_user} -- {} -XwqAt -v ON_ERROR_STOP=1",
            q(&self.args.psql)
        )
    }

    fn admin(&mut self, db: &str, query: &str) -> Result<String> {
        self.run(&format!(
            "{} -d {db} -c {}",
            self.psql("postgres"),
            q(query)
        ))
    }

    fn refused(&mut self, name: &str, os_user: &str, args: &str, diagnostic: &str) -> Result<()> {
        let (code, output) = self.exec(&format!("{} -U {ROLE} {args} 2>&1", self.psql(os_user)))?;
        if code == 0 || !output.contains(diagnostic) {
            return Err(invalid(format!("{name}: exit {code}: {output}")));
        }
        self.check(name, true)
    }

    fn wait(&mut self, script: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (status, output) = self.exec(script)?;
            if status == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(invalid(format!("VM wait timed out: {script}: {output}")));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn observe(&mut self, outcome: &str, previous: Option<&str>) -> Result<Value> {
        let previous_arg =
            previous.map_or_else(String::new, |name| format!(" --previous {DIR}/{name}"));
        let raw = self.run(&format!(
            "umask 022; runuser -u {OBSERVER} -- env PGSERVICE=bogus PGHOSTADDR=127.0.0.1 PGOPTIONS=bogus \
             harbor-db-postgres observe --psql {} --commit-phase-seconds 2 --idle-in-transaction-seconds 1\
             {previous_arg} > {DIR}/{outcome}; cat {DIR}/{outcome}",
            q(&self.args.psql),
        ))?;
        if raw.contains(SENTINEL) || raw.contains("\"query\":") {
            return Err(invalid("observer disclosed raw SQL"));
        }
        Ok(serde_json::from_str(&raw)?)
    }
}

fn finding<'a>(report: &'a Value, rule: &str, pid: &Value) -> Option<&'a Value> {
    report["open_findings"]
        .as_array()?
        .iter()
        .find(|f| f["rule"] == rule && f["subject"]["pid"] == *pid)
}

fn activity<'a>(report: &'a Value, label: &str) -> Option<&'a Value> {
    report["snapshot"]["backends"]
        .as_array()?
        .iter()
        .find(|b| b["application_name"] == label)
}

fn transition(report: &Value, kind: &str, open: &Value) -> bool {
    report["transitions"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|t| t["kind"] == kind && t["finding"] == *open)
    })
}

fn run(args: &Args, client: Client) -> Result<()> {
    let mut g = Gate {
        args,
        client,
        passed: vec![],
    };
    let roles = g.admin("postgres", "
        SELECT NOT rolcanlogin FROM pg_roles WHERE rolname = 'pg_monitor';
        SELECT rolcanlogin AND NOT (rolsuper OR rolreplication OR rolcreatedb OR rolcreaterole OR rolbypassrls)
            AND pg_has_role(rolname, 'pg_monitor', 'MEMBER')
        FROM pg_roles WHERE rolname = 'harbor_db_observer';
    ")?;
    g.check(
        "pg-monitor-is-unaltered-and-login-is-unprivileged",
        roles == "t\nt",
    )?;
    for db in ["postgres", "appdb"] {
        g.admin(db, "CREATE TABLE private_observer_fixture (secret text); REVOKE ALL ON private_observer_fixture FROM PUBLIC;")?;
    }
    let local = g.run(&format!(
        "{} -U {ROLE} -d postgres -c 'SELECT current_user'",
        g.psql(OBSERVER)
    ))?;
    g.check("dedicated-account-connects-by-peer", local == ROLE)?;
    g.refused(
        "tcp-denied",
        OBSERVER,
        "-h 127.0.0.1 -d postgres -c 'SELECT 1'",
        "pg_hba.conf rejects connection",
    )?;
    g.refused(
        "other-database-denied",
        OBSERVER,
        "-d appdb -c 'SELECT 1'",
        "pg_hba.conf rejects connection",
    )?;
    g.refused(
        "physical-replication-denied",
        OBSERVER,
        "-d 'dbname=postgres replication=true' -c IDENTIFY_SYSTEM",
        "pg_hba.conf rejects replication connection",
    )?;
    g.refused(
        "logical-replication-denied",
        OBSERVER,
        "-d 'dbname=postgres replication=database' -c IDENTIFY_SYSTEM",
        "permission denied to start WAL sender",
    )?;
    g.refused(
        "private-table-unreadable",
        OBSERVER,
        "-d postgres -c 'SELECT * FROM private_observer_fixture'",
        "permission denied for table",
    )?;
    g.refused(
        "wrong-os-user-denied",
        "nobody",
        "-d postgres -c 'SELECT 1'",
        "Peer authentication failed",
    )?;

    g.admin(
        "postgres",
        "
        CREATE TABLE deferred_trigger_fixture (id int);
        CREATE FUNCTION busy_at_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE deadline timestamptz := clock_timestamp() + interval '60 seconds';
        BEGIN
            LOOP
                FOR i IN 1..50000 LOOP
                    NULL;
                END LOOP;
                IF pg_try_advisory_lock(424242) THEN
                    PERFORM pg_advisory_unlock(424242);
                    RETURN NEW;
                END IF;
                IF clock_timestamp() >= deadline THEN
                    RAISE EXCEPTION 'fixture advisory lock was not released';
                END IF;
            END LOOP;
        END $$;
        CREATE CONSTRAINT TRIGGER busy_at_commit
            AFTER INSERT ON deferred_trigger_fixture DEFERRABLE INITIALLY DEFERRED
            FOR EACH ROW EXECUTE FUNCTION busy_at_commit();
    ",
    )?;
    let control = format!(
        "{{ printf '%s\\n' 'SELECT pg_advisory_lock(424242);'; {} {DIR}/release; }} \
         | {} -Xwq -v ON_ERROR_STOP=1 -d postgres",
        q(&args.cat),
        q(&args.psql)
    );
    g.run(&format!(
        "install -d -m 0755 {DIR}; mkfifo -m 0644 {DIR}/release; \
         systemd-run --unit=commit-control-fixture --uid=postgres --setenv=PGAPPNAME=commit-control \
         --property=RemainAfterExit=yes --property=RuntimeMaxSec=75s {} -c {}",
        q(&args.shell),
        q(&control)
    ))?;
    g.wait(&format!(
        "test \"$({} -d postgres -c \"SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING (pid) \
         WHERE application_name='commit-control' AND locktype='advisory' AND granted \
         AND classid=0 AND objid=424242 AND objsubid=1\")\" = 1",
        g.psql("postgres")
    ))?;
    g.run(&format!("umask 022; \
        printf '%s\\n' 'BEGIN;' 'INSERT INTO deferred_trigger_fixture VALUES (1);' \
        '/* outer /* inner */ */ -- {SENTINEL}' ' COMMIT;' > {DIR}/commit.sql; \
        systemd-run --unit=deferred-trigger-fixture --uid=postgres --setenv=PGAPPNAME=deferred-trigger-test \
        --property=RemainAfterExit=yes --property=RuntimeMaxSec=70s \
        {} -Xwq -v ON_ERROR_STOP=1 -d postgres -f {DIR}/commit.sql",
        q(&args.psql),
    ))?;
    let committing = format!(
        "SELECT count(*) FROM pg_stat_activity WHERE application_name='deferred-trigger-test' \
         AND state='active' AND query LIKE '/* outer /* inner */ */ -- {SENTINEL}%'"
    );
    let psql = g.psql("postgres");
    g.wait(&format!(
        "test \"$({psql} -d postgres -c {})\" = 1",
        q(&committing)
    ))?;
    let first = g.observe("first.json", None)?;
    let row = activity(&first, "deferred-trigger-test")
        .ok_or_else(|| invalid("first COMMIT session missing"))?;
    g.check(
        "observer-sees-busy-commit-metadata",
        row["state"] == "active" && row["is_commit"] == true && row["wait_event_type"].is_null(),
    )?;
    g.wait(&format!(
        "test \"$({psql} -d postgres -c {})\" = 1",
        q(&format!(
            "{committing} AND clock_timestamp()-query_start > interval '2 seconds'"
        ))
    ))?;
    let second = g.observe("second.json", Some("first.json"))?;
    let open = finding(&second, "commit-phase", &row["pid"])
        .ok_or_else(|| invalid("commit-phase did not fire on the second snapshot"))?;
    g.check("deferred-trigger-commit-finding-has-transaction-identity-no-sql", open["hint"] == "deferred triggers"
        && open["subject"] == json!({"kind": "backend", "pid": row["pid"], "backend_start": row["backend_start"], "xact_start": row["xact_start"]}))?;
    g.run(&format!(
        "timeout 5 {} -c {}",
        q(&args.shell),
        q(&format!(
            "printf '%s\\n' 'SELECT pg_advisory_unlock(424242);' > {DIR}/release"
        ))
    ))?;
    g.wait(
        "for unit in commit-control-fixture deferred-trigger-fixture; do \
        test \"$(systemctl show \"$unit\" -p SubState --value)\" = exited && \
        test \"$(systemctl show \"$unit\" -p Result --value)\" = success || exit 1; done",
    )?;
    let count = g.admin("postgres", "SELECT count(*) FROM deferred_trigger_fixture")?;
    let third = g.observe("third.json", Some("second.json"))?;
    g.check(
        "commit-completes-and-finding-resolves",
        count == "1"
            && finding(&third, "commit-phase", &row["pid"]).is_none()
            && transition(&third, "resolved", open),
    )?;

    let idle_client = format!(
        "{{ printf 'BEGIN;\\n'; {} 45; }} | {} -Xwq -v ON_ERROR_STOP=1 -d postgres",
        q(&args.sleep),
        q(&args.psql)
    );
    g.run(&format!("systemd-run --unit=idle-transaction-fixture --uid=postgres --setenv=PGAPPNAME=idle-transaction-test \
        --property=RuntimeMaxSec=50s {} -c {}", q(&args.shell), q(&idle_client)))?;
    g.wait(&format!(
        "test \"$({} -d postgres -c \"SELECT count(*) FROM pg_stat_activity \
        WHERE application_name='idle-transaction-test' AND state='idle in transaction' \
        AND clock_timestamp()-state_change > interval '1 second'\")\" = 1",
        g.psql("postgres")
    ))?;
    let before = g.observe("before-restart.json", None)?;
    let idle = activity(&before, "idle-transaction-test")
        .and_then(|row| finding(&before, "idle-in-transaction", &row["pid"]))
        .ok_or_else(|| invalid("idle transaction finding missing before restart"))?;
    g.run("systemctl restart postgresql.service; systemctl start postgresql-setup.service; systemctl is-active postgresql-setup.service")?;
    let after = g.observe("after-restart.json", Some("before-restart.json"))?;
    g.check(
        "restart-ends-rather-than-resolves-old-finding",
        after["state"]["postmaster_start_time"] != before["state"]["postmaster_start_time"]
            && transition(&after, "ended", idle)
            && !transition(&after, "resolved", idle),
    )?;
    durable::write_json(
        &args.acceptance,
        &json!({
            "schema": 1,
            "case_id": harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-postgres-observer"),
            "assertions": g.passed,
        }),
    )
}

fn main() -> Result<()> {
    let args = Args::parse();
    // SAFETY: the test bridge passes ownership of an inherited Unix socket.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    run(&args, Client::new(stream, Duration::from_secs(65))?)
}
