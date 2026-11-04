#![cfg(target_os = "linux")]
//! The actual `harbor-db-rehearse` CLI against real, disposable PostgreSQL
//! clusters: a source database built here and a rehearsal cluster built by the
//! engine.
use harbor_db::storage::codec;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Source {
    root: tempfile::TempDir,
    package: PathBuf,
    socket: PathBuf,
}

impl Source {
    /// A running socket-only cluster (default port, so `host` alone reaches it)
    /// holding `srcdb`, owned by the non-superuser `src_owner`, with a superuser
    /// extension and one table. `postgis` adds the untrusted extension.
    fn new(postgis: bool) -> Self {
        let package = PathBuf::from(
            std::env::var_os("HARBOR_DB_TEST_POSTGRES").expect("qualified PostgreSQL package"),
        );
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("src-socket");
        fs::create_dir(&socket).unwrap();
        let source = Self {
            root,
            package,
            socket,
        };
        let data = source.root.path().join("src-cluster");
        source.pg(
            "initdb",
            &["-D", data.to_str().unwrap(), "--locale=C", "--auth=trust"],
        );
        source.pg(
            "pg_ctl",
            &[
                "-D",
                data.to_str().unwrap(),
                "-l",
                "/dev/null",
                "-o",
                &format!("-k {} -c listen_addresses=", source.socket.display()),
                "-w",
                "start",
            ],
        );
        source.sql(None, "postgres", "CREATE ROLE src_owner LOGIN NOSUPERUSER; CREATE ROLE reader NOLOGIN; CREATE ROLE etl_default NOLOGIN");
        source.sql(
            None,
            "postgres",
            "CREATE DATABASE srcdb OWNER src_owner TEMPLATE template0 LOCALE 'C'",
        );
        source.sql(None, "srcdb", "CREATE EXTENSION pg_trgm");
        if postgis {
            source.sql(None, "srcdb", "CREATE EXTENSION postgis");
        }
        source.sql(
            Some("src_owner"),
            "srcdb",
            "CREATE TABLE records(id int PRIMARY KEY, note text); INSERT INTO records VALUES (1,'alpha'),(2,'beta'); GRANT SELECT ON records TO reader; ALTER DEFAULT PRIVILEGES FOR ROLE src_owner IN SCHEMA public GRANT SELECT ON TABLES TO etl_default",
        );
        source
    }

    fn pg(&self, program: &str, args: &[&str]) -> String {
        let output = Command::new(self.package.join("bin").join(program))
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn sql(&self, user: Option<&str>, database: &str, sql: &str) -> String {
        let mut args = vec!["-X", "-At", "-v", "ON_ERROR_STOP=1", "-h"];
        let socket = self.socket.display().to_string();
        args.push(&socket);
        args.extend(user.iter().flat_map(|user| ["-U", user]));
        args.extend(["-d", database, "-c", sql]);
        self.pg("psql", &args).trim().to_string()
    }

    /// Dump `srcdb`; `directory` selects `-Fd`, otherwise a custom-format file.
    fn dump(&self, directory: bool) -> PathBuf {
        let path = self
            .root
            .path()
            .join(if directory { "dump-dir" } else { "dump.custom" });
        let format = if directory { "-Fd" } else { "-Fc" };
        self.pg(
            "pg_dump",
            &[
                format,
                "-h",
                self.socket.to_str().unwrap(),
                "-d",
                "srcdb",
                "-f",
                path.to_str().unwrap(),
            ],
        );
        path
    }

    fn work(&self) -> PathBuf {
        let work = self.root.path().join("work");
        fs::create_dir_all(&work).unwrap();
        work
    }

    fn plan(&self, source: Value, work: &Path) -> Value {
        json!({
            "schema": "harbor-db.rehearsal-plan.v1",
            "package": self.package,
            "source": source,
            "owner": "src_owner",
            "database": "rehearsed",
            "locale": "C",
            "work_directory": work,
            "jobs": 2,
            "timeout_seconds": 300,
        })
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let data = self.root.path().join("src-cluster");
        let _ = Command::new(self.package.join("bin/pg_ctl"))
            .args(["-D", data.to_str().unwrap(), "-m", "immediate", "stop"])
            .output();
    }
}

struct Run {
    output: Output,
    receipt: Option<Value>,
    receipt_path: Option<PathBuf>,
    receipt_bytes: Vec<u8>,
}

fn rehearse(source: &Source, plan: &Value) -> Run {
    let plan_path = source.root.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec(plan).unwrap()).unwrap();
    let receipts = source.root.path().join("rehearsals");
    let output = Command::new(env!("CARGO_BIN_EXE_harbor-db-rehearse"))
        .args(["--plan", plan_path.to_str().unwrap(), "--receipt-dir"])
        .arg(&receipts)
        // The scrubbed step environment must not inherit this.
        .env("HARBOR_LEAK", "1")
        .output()
        .unwrap();
    let published: Option<Value> = serde_json::from_slice(&output.stdout).ok();
    let receipt_path = published
        .as_ref()
        .map(|p| PathBuf::from(p["receipt"].as_str().unwrap()));
    let receipt_bytes = receipt_path
        .as_ref()
        .map(|path| fs::read(path).unwrap())
        .unwrap_or_default();
    if let Some(published) = &published {
        assert_eq!(
            published["receipt_sha256"],
            codec::digest(&receipt_bytes),
            "published digest must match the receipt bytes"
        );
    }
    Run {
        output,
        receipt: serde_json::from_slice(&receipt_bytes).ok(),
        receipt_path,
        receipt_bytes,
    }
}

fn step_by<'a>(receipt: &'a Value, name: &str) -> &'a Value {
    receipt["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["name"] == name)
        .unwrap_or_else(|| panic!("no step {name}: {receipt}"))
}

fn probe<'a>(receipt: &'a Value, name: &str) -> &'a str {
    receipt["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|probe| probe["name"] == name)
        .unwrap_or_else(|| panic!("no probe {name}: {receipt}"))["result"]
        .as_str()
        .unwrap_or_else(|| panic!("probe {name} has no result: {receipt}"))
}

const PSQL: &str = "psql -v ON_ERROR_STOP=1 -qAt";

fn migrate_check_maintain(plan: &mut Value) {
    // fuzzystrmatch is needed by the "migration" only; pg_trgm comes from the dump TOC.
    plan["superuser_extensions"] = json!(["fuzzystrmatch"]);
    plan["steps"] = json!([
        {"name": "migrate", "kind": "migrate", "argv": ["sh", "-c",
            format!("{PSQL} -c 'CREATE TABLE migrated(id int); CREATE INDEX ON records USING gin (note gin_trgm_ops)'")]},
        {"name": "check", "kind": "check", "env": {"FLAVOUR": "chocolate"}, "argv": ["sh", "-c",
            format!("test \"$({PSQL} -c 'select count(*) from records')\" = 2 && test -z \"${{HARBOR_LEAK:-}}\" && test \"$FLAVOUR\" = chocolate && test \"$PGUSER\" = src_owner && echo \"$DATABASE_URL\"")]},
        {"name": "reindex", "kind": "maintenance", "expect_exit": 3, "argv": ["sh", "-c",
            format!("{PSQL} -c 'REINDEX TABLE records' && echo reindexed; exit 3")]},
    ]);
    plan["probes"] = json!([
        {"name": "rows", "sql": "select count(*) from records"},
        {"name": "owners", "sql": "select string_agg(distinct tableowner, ',') from pg_tables where schemaname = 'public'"},
        {"name": "grants_kept", "sql": "select has_table_privilege('reader', 'records', 'SELECT') and not has_table_privilege('etl_default', 'records', 'INSERT')"},
        {"name": "standins_cannot_login", "sql": "select count(*) from pg_roles where rolname in ('reader', 'etl_default') and not rolcanlogin"},
        {"name": "owner_may_use_the_new_extension", "sql": "select levenshtein('kitten', 'sitting')"},
        {"name": "owner_is_superuser", "sql": "select rolsuper from pg_roles where rolname = current_user"},
        {"name": "extension_owner_is_not_the_owner", "sql": "select count(*) from pg_extension e join pg_roles r on r.oid = e.extowner where e.extname = 'pg_trgm' and r.rolname <> 'src_owner'"},
    ]);
}

#[test]
fn dump_rehearsal_runs_steps_as_the_owner_and_leaves_nothing_behind() {
    let source = Source::new(false);
    let work = source.work();
    let dump = source.dump(true);
    let mut plan = source.plan(json!({"dump": dump}), &work);
    migrate_check_maintain(&mut plan);
    let run = rehearse(&source, &plan);
    let receipt = run.receipt.clone().expect("receipt");
    assert!(
        run.output.status.success(),
        "{receipt:#}\n{}",
        String::from_utf8_lossy(&run.output.stderr)
    );

    assert_eq!(receipt["schema"], "harbor-db.rehearsal-receipt.v1");
    assert_eq!(receipt["status"], "passed");
    assert_eq!(
        receipt["plan_sha256"],
        codec::digest(&fs::read(source.root.path().join("plan.json")).unwrap())
    );
    assert_eq!(receipt["package"], source.package.to_str().unwrap());
    assert_eq!(receipt["source"]["kind"], "dump");
    assert_eq!(receipt["source"]["database"], "srcdb");
    assert!(receipt["source"]["dump_bytes"].as_u64().unwrap() > 0);
    assert_eq!(receipt["source"]["toc_sha256"].as_str().unwrap().len(), 64);
    assert!(
        receipt["source"]["server_version"]
            .as_str()
            .unwrap()
            .starts_with(char::is_numeric)
    );
    assert!(receipt["restore_seconds"].as_f64().unwrap() > 0.0);
    assert!(
        receipt["total_seconds"].as_f64().unwrap() >= receipt["restore_seconds"].as_f64().unwrap()
    );
    assert!(receipt["failure"].is_null());
    assert_eq!(receipt["cleanup"], "removed");

    let order: Vec<_> = receipt["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["migrate", "check", "reindex"]);
    assert_eq!(step_by(&receipt, "migrate")["kind"], "migrate");
    assert_eq!(step_by(&receipt, "migrate")["exit_code"], 0);
    assert_eq!(step_by(&receipt, "migrate")["argv"][0], "sh");
    let check = step_by(&receipt, "check");
    assert_eq!(check["kind"], "check");
    assert!(check["seconds"].as_f64().is_some());
    let url = check["stdout_tail"].as_str().unwrap();
    assert!(url.starts_with("postgres:///rehearsed?host="), "{url}");
    assert!(url.contains("&port=55439&user=src_owner"), "{url}");
    let reindex = step_by(&receipt, "reindex");
    assert_eq!(
        (
            reindex["kind"].as_str(),
            reindex["exit_code"].as_i64(),
            reindex["expect_exit"].as_i64()
        ),
        (Some("maintenance"), Some(3), Some(3))
    );
    assert!(
        reindex["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("reindexed")
    );

    assert_eq!(probe(&receipt, "rows"), "2");
    // Restored and migrated objects belong to the non-superuser owner.
    assert_eq!(probe(&receipt, "owners"), "src_owner");
    assert_eq!(probe(&receipt, "owner_is_superuser"), "f");
    // Production ACLs are restored exactly, through NOLOGIN stand-in grantees.
    assert_eq!(receipt["standin_roles"], json!(["etl_default", "reader"]));
    assert_eq!(probe(&receipt, "grants_kept"), "t");
    assert_eq!(probe(&receipt, "standins_cannot_login"), "2");
    // Source extensions plus the migration's own, created by the superuser.
    assert_eq!(receipt["extensions"], json!(["fuzzystrmatch", "pg_trgm"]));
    assert_eq!(probe(&receipt, "owner_may_use_the_new_extension"), "3");
    // The superuser's extension survived the filtered restore untouched.
    assert_eq!(probe(&receipt, "extension_owner_is_not_the_owner"), "1");

    // The disposable cluster and work directory are gone; the source is untouched.
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0);
    assert_eq!(
        source.sql(
            None,
            "srcdb",
            "select count(*) from pg_tables where tablename = 'migrated'"
        ),
        "0"
    );
    assert!(
        run.receipt_path
            .unwrap()
            .extension()
            .is_some_and(|e| e == "json")
    );
}

#[test]
fn live_rehearsal_failing_step_stops_the_run_and_keep_retains_a_stopped_cluster() {
    let source = Source::new(false);
    let work = source.work();
    let mut plan = source.plan(
        json!({"live": {"host": source.socket, "database": "srcdb", "user": "src_owner"}}),
        &work,
    );
    plan.as_object_mut().unwrap().remove("locale");
    plan["keep"] = json!(true);
    plan["steps"] = json!([
        {"name": "fine", "kind": "migrate", "argv": ["true"]},
        {"name": "bad", "kind": "check", "argv": ["sh", "-c", "echo boom >&2; exit 3"]},
        {"name": "never", "kind": "maintenance", "argv": ["sh", "-c", format!("{PSQL} -c 'CREATE TABLE never(id int)'")]},
    ]);
    plan["probes"] = json!([{"name": "rows", "sql": "select count(*) from records"}]);
    let run = rehearse(&source, &plan);
    let receipt = run
        .receipt
        .clone()
        .expect("a failed rehearsal still publishes a receipt");
    assert_eq!(run.output.status.code(), Some(1));
    assert_eq!(receipt["status"], "failed");
    assert!(
        receipt["failure"]
            .as_str()
            .unwrap()
            .contains("step bad ended with exit 3")
    );
    assert_eq!(
        receipt["steps"].as_array().unwrap().len(),
        2,
        "later steps must not run"
    );
    let bad = step_by(&receipt, "bad");
    assert_eq!(
        (bad["exit_code"].as_i64(), bad["expect_exit"].as_i64()),
        (Some(3), Some(0))
    );
    assert!(bad["stderr_tail"].as_str().unwrap().contains("boom"));
    assert!(
        receipt["probes"].as_array().unwrap().is_empty(),
        "probes follow successful steps only"
    );

    // Live source identity: dumped with pg_dump, locale defaulted from datcollate.
    assert_eq!(receipt["source"]["kind"], "live");
    assert_eq!(receipt["source"]["database"], "srcdb");
    assert_eq!(receipt["locale"], "C");
    assert!(receipt["dump_seconds"].as_f64().is_some());
    assert!(receipt["source"]["dump_bytes"].as_u64().unwrap() > 0);
    assert_eq!(receipt["source"]["toc_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(
        source.sql(
            None,
            "srcdb",
            "select count(*) from pg_tables where tablename in ('never', 'migrated')"
        ),
        "0"
    );

    // keep: the directory stays, the cluster does not run.
    assert!(
        receipt["cleanup"]
            .as_str()
            .unwrap()
            .contains("work directory kept")
    );
    let kept: Vec<_> = fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1);
    let status = Command::new(source.package.join("bin/pg_ctl"))
        .args(["-D", kept[0].join("cluster").to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert_eq!(
        status.status.code(),
        Some(3),
        "pg_ctl status 3 means not running"
    );
    assert!(!kept[0].join("cluster/postmaster.pid").exists());
}

#[test]
fn probes_are_read_only_and_a_failing_probe_fails_the_rehearsal() {
    let source = Source::new(false);
    let work = source.work();
    let dump = source.dump(false);
    let mut plan = source.plan(json!({"dump": dump}), &work);
    plan["probes"] = json!([
        {"name": "ok", "sql": "select 1"},
        {"name": "write", "sql": "insert into records values (9, 'x')"},
    ]);
    let run = rehearse(&source, &plan);
    let receipt = run.receipt.expect("receipt");
    assert_eq!(run.output.status.code(), Some(1));
    assert_eq!(receipt["status"], "failed");
    assert_eq!(probe(&receipt, "ok"), "1");
    assert!(
        receipt["probes"][1]["error"]
            .as_str()
            .unwrap()
            .contains("read-only")
    );
    assert!(
        receipt["failure"]
            .as_str()
            .unwrap()
            .contains("probe write failed")
    );
    assert_eq!(receipt["cleanup"], "removed");
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0);
}

#[test]
fn untrusted_superuser_extension_is_created_before_the_owner_migrates() {
    let package =
        std::env::var_os("HARBOR_DB_TEST_POSTGRES").expect("qualified PostgreSQL package");
    if !Path::new(&package)
        .join("share/postgresql/extension/postgis.control")
        .is_file()
    {
        // Hosts without postgis in the test package get the pg_trgm path above.
        eprintln!(
            "SKIP: HARBOR_DB_TEST_POSTGRES has no postgis; the untrusted-extension case did not run"
        );
        return;
    }
    let source = Source::new(true);
    source.sql(
        Some("src_owner"),
        "srcdb",
        "CREATE TABLE places(g geometry(Point, 4326))",
    );
    // An ACL on an extension-owned object, and the extension's config-table data
    // that the owner may not restore, both appear in the dump.
    source.sql(
        None,
        "srcdb",
        "GRANT SELECT ON spatial_ref_sys TO etl_default",
    );
    let work = source.work();
    let dump = source.dump(false);
    let mut plan = source.plan(json!({"dump": dump}), &work);
    plan["superuser_extensions"] = json!(["postgis"]);
    plan["steps"] = json!([{"name": "migrate", "kind": "migrate", "argv": ["sh", "-c",
        format!("{PSQL} -c 'CREATE TABLE migrated_places(g geometry(Point, 4326))'")]}]);
    plan["probes"] = json!([
        {"name": "postgis", "sql": "select postgis_version() is not null"},
        {"name": "owners", "sql": "select string_agg(distinct tableowner, ',') from pg_tables where tablename in ('places', 'migrated_places')"},
    ]);
    let run = rehearse(&source, &plan);
    assert!(
        run.output.status.success(),
        "{}",
        String::from_utf8_lossy(&run.receipt_bytes)
    );
    let receipt = run.receipt.unwrap();
    assert_eq!(probe(&receipt, "postgis"), "t");
    assert_eq!(probe(&receipt, "owners"), "src_owner");
}

#[test]
fn unsafe_work_directory_and_bad_plans_are_refused_before_any_mutation() {
    let source = Source::new(false);
    let dump = source.dump(true);
    // The work directory contains the dump: cleanup would delete the source.
    let plan = source.plan(json!({"dump": dump}), source.root.path());
    let run = rehearse(&source, &plan);
    assert_eq!(run.output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.output.stderr).contains("overlaps the source"));
    assert!(dump.join("toc.dat").is_file());
    assert!(!source.root.path().join("rehearsals").exists());

    let work = source.work();
    let mut plan = source.plan(json!({"dump": dump}), &work);
    plan["steps"] = json!([{"name": "s", "kind": "check", "argv": ["true"], "env": {"PGHOST": "/run/postgresql"}}]);
    let run = rehearse(&source, &plan);
    assert_eq!(run.output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&run.output.stderr).contains("cannot set environment variable")
    );
    plan["steps"] = json!([]);
    plan["surprise"] = json!(true);
    let run = rehearse(&source, &plan);
    assert!(String::from_utf8_lossy(&run.output.stderr).contains("unknown field"));
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0);
    assert!(!source.root.path().join("rehearsals").exists());
}
