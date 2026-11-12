//! Rehearse migrations and maintenance tasks on a restored copy of a database.
//!
//! A plan (`harbor-db.rehearsal-plan.v1`) names a source (a live database read
//! with `pg_dump`, or an existing dump), the owner role and the argv steps and
//! SQL probes to run. The engine restores the source into the private
//! Unix-socket-only drill cluster, runs the steps as the non-superuser owner,
//! and publishes a `harbor-db.rehearsal-receipt.v1` receipt. The source is
//! only ever read, and the disposable cluster is stopped on every exit.
use super::{Result, codec, durable, invalid, pg_core, postgres_drill, process};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const PLAN_SCHEMA: &str = "harbor-db.rehearsal-plan.v1";
pub const RECEIPT_SCHEMA: &str = "harbor-db.rehearsal-receipt.v1";
const PORT: &str = "55439";
const DEFAULT_TIMEOUT_SECONDS: u64 = 14400;
const CLEANUP_SECONDS: u64 = 120;
const TAIL_BYTES: usize = 4096;
const PROBE_BYTES: usize = 16384;
const PLAN_LIMIT: u64 = 1 << 20;
/// A rehearsal never needs durability, only speed; the cluster is thrown away.
const SERVER_SETTINGS: [&str; 5] = [
    "fsync=off",
    "synchronous_commit=off",
    "full_page_writes=off",
    "max_wal_size=16GB",
    "maintenance_work_mem=1GB",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: String,
    pub package: PathBuf,
    pub source: Source,
    pub owner: String,
    /// Bootstrap superuser `initdb` creates. Defaults to the account running
    /// the engine; the module sets a distinct name so a project owner that is
    /// also the unit's account can still be restored and rehearsed.
    #[serde(default)]
    pub superuser: Option<String>,
    pub database: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub superuser_extensions: Vec<String>,
    /// Extra roles to stand in (NOLOGIN) for grantees the dump references.
    #[serde(default)]
    pub grantee_roles: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub probes: Vec<Probe>,
    pub work_directory: PathBuf,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub jobs: Option<u32>,
    #[serde(default)]
    pub keep: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Live(Live),
    Dump(PathBuf),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Live {
    pub host: String,
    pub database: String,
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Migrate,
    Check,
    Maintenance,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub kind: Kind,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub expect_exit: i32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub name: String,
    pub sql: String,
}

#[derive(Clone, Default, Serialize)]
struct SourceReport {
    kind: &'static str,
    database: String,
    server_version: Option<String>,
    dump_bytes: Option<u64>,
    toc_sha256: Option<String>,
}

#[derive(Serialize)]
struct StepReport {
    name: String,
    kind: Kind,
    argv: Vec<String>,
    expect_exit: i32,
    exit_code: Option<i32>,
    seconds: f64,
    error: Option<String>,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Serialize)]
struct ProbeReport {
    name: String,
    sql: String,
    result: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Receipt {
    schema: &'static str,
    status: &'static str,
    plan_sha256: String,
    started_at: String,
    package: String,
    owner: String,
    database: String,
    locale: String,
    source: SourceReport,
    extensions: Vec<String>,
    standin_roles: Vec<String>,
    dump_seconds: Option<f64>,
    restore_seconds: Option<f64>,
    steps: Vec<StepReport>,
    probes: Vec<ProbeReport>,
    failure: Option<String>,
    cleanup: String,
    total_seconds: f64,
}

pub struct Outcome {
    pub receipt: PathBuf,
    pub receipt_sha256: String,
    pub passed: bool,
}

fn seconds(since: Instant) -> f64 {
    (since.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

fn name_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 63 && !value.contains(['\0', '\n', '\r'])
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn parse_plan(bytes: &[u8]) -> Result<Plan> {
    let plan: Plan = serde_json::from_slice(bytes)?;
    if plan.schema != PLAN_SCHEMA {
        return Err(invalid(format!(
            "rehearsal plan schema must be {PLAN_SCHEMA}"
        )));
    }
    if !plan.package.is_absolute() || !plan.work_directory.is_absolute() {
        return Err(invalid(
            "rehearsal package and work_directory must be absolute",
        ));
    }
    for program in ["initdb", "pg_ctl", "pg_restore", "pg_dump", "psql"] {
        if !plan.package.join("bin").join(program).is_file() {
            return Err(invalid(format!("rehearsal package lacks bin/{program}")));
        }
    }
    let mut names = vec![&plan.owner, &plan.database];
    names.extend(&plan.superuser);
    names.extend(&plan.superuser_extensions);
    names.extend(&plan.grantee_roles);
    if let Source::Live(live) = &plan.source {
        names.extend([&live.database]);
        if live.host.is_empty() || live.host.starts_with('-') {
            return Err(invalid("invalid live source host"));
        }
        if live.user.as_ref().is_some_and(|user| !name_ok(user)) {
            return Err(invalid("invalid live source user"));
        }
    }
    if names.iter().any(|name| !name_ok(name))
        || plan.locale.as_deref().is_some_and(|locale| {
            locale.is_empty()
                || !locale
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b))
        })
    {
        return Err(invalid(
            "invalid rehearsal role, database, extension or locale name",
        ));
    }
    if plan.jobs.is_some_and(|jobs| !(1..=64).contains(&jobs)) {
        return Err(invalid("rehearsal jobs must be 1..=64"));
    }
    if plan.superuser.as_deref() == Some(plan.owner.as_str()) {
        return Err(invalid(
            "rehearsal owner must not be the cluster superuser account",
        ));
    }
    let mut seen = BTreeSet::new();
    for step in &plan.steps {
        if !name_ok(&step.name) || !seen.insert(&step.name) {
            return Err(invalid("rehearsal step names must be unique and non-empty"));
        }
        if step.argv.first().is_none_or(String::is_empty) {
            return Err(invalid(format!(
                "rehearsal step {} has no command",
                step.name
            )));
        }
        // A step must not be able to retarget the disposable connection.
        if let Some(key) = step.env.keys().find(|key| {
            key.is_empty()
                || key.contains(['=', '\0'])
                || *key == "DATABASE_URL"
                || key.starts_with("PG")
        }) {
            return Err(invalid(format!(
                "rehearsal step {} cannot set environment variable {key:?}",
                step.name
            )));
        }
    }
    let mut seen = BTreeSet::new();
    if plan
        .probes
        .iter()
        .any(|p| !name_ok(&p.name) || !seen.insert(&p.name))
    {
        return Err(invalid(
            "rehearsal probe names must be unique and non-empty",
        ));
    }
    Ok(plan)
}

fn is_default_acl(line: &str) -> bool {
    let words: Vec<_> = line.split_whitespace().collect();
    !line.starts_with(';') && words.get(3) == Some(&"DEFAULT") && words.get(4) == Some(&"ACL")
}

/// `ALTER DEFAULT PRIVILEGES FOR ROLE x` needs to be x or a superuser, so the
/// owner-role pass leaves these entries to a second superuser pass.
fn default_acl_toc(toc: &str) -> String {
    toc.lines()
        .filter(|line| is_default_acl(line))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// The owner-role pass's list: no extension entries (the superuser already
/// created them), no extension member data, no default ACLs.
fn filter_toc(toc: &str, extensions: &[String], config_tables: &str) -> String {
    let skip: Vec<Vec<&str>> = config_tables
        .lines()
        .map(|line| line.split(' ').collect())
        .collect();
    toc.lines()
        .filter(|line| !is_default_acl(line))
        .filter(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            !words.windows(4).any(|w| {
                w[0] == "TABLE" && w[1] == "DATA" && skip.iter().any(|table| table[..] == w[2..])
            }) && !words
                .windows(3)
                .any(|w| w[0] == "EXTENSION" && w[1] == "-" && extensions.iter().any(|e| e == w[2]))
                && !words.windows(4).any(|w| {
                    w[0] == "COMMENT"
                        && w[1] == "-"
                        && w[2] == "EXTENSION"
                        && extensions.iter().any(|e| e == w[3])
                })
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Names of the extensions a dump's TOC creates.
fn toc_extensions(toc: &str) -> BTreeSet<String> {
    toc.lines()
        .filter(|line| !line.starts_with(';'))
        .filter_map(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            words
                .windows(3)
                .find(|w| w[0] == "EXTENSION" && w[1] == "-")
                .map(|w| w[2].to_string())
        })
        .collect()
}

/// The TOC lines for ACL and default-ACL entries, which carry the grantees.
fn acl_toc(toc: &str) -> String {
    toc.lines()
        .filter(|line| !line.starts_with(';'))
        .filter(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            words.get(3) == Some(&"ACL")
                || (words.get(3) == Some(&"DEFAULT") && words.get(4) == Some(&"ACL"))
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

fn role_name(token: &str) -> Option<String> {
    let token = token.trim();
    let name = match token.strip_prefix('"') {
        Some(quoted) => quoted.strip_suffix('"')?.replace("\"\"", "\""),
        // PUBLIC and the CURRENT_/SESSION_ keywords name no real role.
        None if ["PUBLIC", "CURRENT_USER", "CURRENT_ROLE", "SESSION_USER"]
            .iter()
            .any(|keyword| token.eq_ignore_ascii_case(keyword)) =>
        {
            return None;
        }
        None => token.to_string(),
    };
    (!name.is_empty()).then_some(name)
}

/// Roles named by the GRANT, REVOKE and ALTER DEFAULT PRIVILEGES statements
/// pg_restore prints for ACL entries: grantees plus `FOR ROLE` targets.
/// ponytail: line-wise text scan that assumes role names contain no ` TO `,
/// ` FROM `, comma or semicolon; a miss fails the strict restore loudly and
/// the plan's `grantee_roles` supplies the stand-in.
fn acl_roles(sql: &str) -> BTreeSet<String> {
    let mut roles = BTreeSet::new();
    for line in sql.lines() {
        let default = line.starts_with("ALTER DEFAULT PRIVILEGES");
        if !(default || line.starts_with("GRANT ") || line.starts_with("REVOKE ")) {
            continue;
        }
        let revoke = line.starts_with("REVOKE ") || (default && line.contains(" REVOKE "));
        let marker = if revoke { " FROM " } else { " TO " };
        if let Some((_, grantees)) = line.rsplit_once(marker) {
            let grantees = grantees.trim_end_matches(';');
            let grantees = grantees
                .split(" WITH GRANT OPTION")
                .next()
                .unwrap_or(grantees);
            roles.extend(grantees.split(',').filter_map(role_name));
        }
        if let Some((_, targets)) = line.split_once(" FOR ROLE ") {
            let end = [" IN SCHEMA ", " GRANT ", " REVOKE "]
                .iter()
                .filter_map(|stop| targets.find(stop))
                .min()
                .unwrap_or(targets.len());
            roles.extend(targets[..end].split(',').filter_map(role_name));
        }
    }
    roles
}

fn toc_header<'a>(toc: &'a str, key: &str) -> Option<&'a str> {
    toc.lines()
        .take_while(|line| line.starts_with(';'))
        .find_map(|line| {
            line.trim_start_matches(';')
                .trim()
                .strip_prefix(key)?
                .strip_prefix(':')
        })
        .map(str::trim)
}

fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]).into_owned()
}

fn file_tail(path: &Path) -> String {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES as u64)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    tail(&read().unwrap_or_default())
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| invalid("rehearsal exceeded its execution limit"))
}

/// Run one PostgreSQL client program and return stdout. Failures carry a
/// bounded stderr tail, unlike the drill's quiet helper.
fn pg(
    package: &Path,
    program: &str,
    args: Vec<String>,
    env: &[(&str, &str)],
    deadline: Instant,
) -> Result<String> {
    let mut argv = vec![package.join("bin").join(program).display().to_string()];
    argv.extend(args);
    let mut spec = pg_core::command(argv, true);
    spec.capture_stderr = true;
    spec.maximum_output = 64 << 20;
    spec.timeout = remaining(deadline)?;
    if let Some(environment) = spec.environment.as_mut() {
        environment.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    }
    let output = process::run(&spec)?;
    if !output.status.success() {
        return Err(invalid(format!(
            "{program} failed ({}): {}",
            output.status,
            tail(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct Cluster<'a> {
    package: &'a Path,
    socket: String,
    user: String,
}

impl Cluster<'_> {
    fn endpoint(&self, user: &str, database: &str) -> Vec<String> {
        ["-h", &self.socket, "-p", PORT, "-U", user, "-d", database]
            .map(String::from)
            .to_vec()
    }

    fn sql(
        &self,
        user: &str,
        database: &str,
        sql: &str,
        env: &[(&str, &str)],
        deadline: Instant,
    ) -> Result<String> {
        let mut args = vec![
            "-X".into(),
            "-qAt".into(),
            "-v".into(),
            "ON_ERROR_STOP=1".into(),
        ];
        args.extend(self.endpoint(user, database));
        args.extend(["-c".into(), sql.into()]);
        pg(self.package, "psql", args, env, deadline)
    }
}

fn percent(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn dump_bytes(path: &Path) -> Result<u64> {
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir() {
        return Ok(info.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += dump_bytes(&entry?.path())?;
    }
    Ok(total)
}

/// Refuse a work directory that overlaps anything the rehearsal must not touch.
fn check_work_directory(work: &Path, protected: &[PathBuf]) -> Result<()> {
    let work = fs::canonicalize(work)?;
    for path in protected {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if work.starts_with(&path) || path.starts_with(&work) {
            return Err(invalid(format!(
                "rehearsal work_directory overlaps the source at {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Run one step with captured, bounded output. The step's process group is
/// killed when the step ends so nothing outlives it holding the connection.
fn run_step(
    step: &Step,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    logs: &Path,
    index: usize,
    deadline: Instant,
) -> Result<StepReport> {
    let (out, err) = (
        logs.join(format!("{index}.out")),
        logs.join(format!("{index}.err")),
    );
    let mut report = StepReport {
        name: step.name.clone(),
        kind: step.kind,
        argv: step.argv.clone(),
        expect_exit: step.expect_exit,
        exit_code: None,
        seconds: 0.0,
        error: None,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    };
    let started = Instant::now();
    let mut command = Command::new(&step.argv[0]);
    command
        .args(&step.argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(File::create(&out)?)
        .stderr(File::create(&err)?)
        .process_group(0);
    match process::spawn(&mut command) {
        Err(error) => report.error = Some(format!("spawn failed: {error}")),
        Ok(mut child) => {
            let pid = child.id() as i32;
            let mut timed_out = false;
            loop {
                // SAFETY: zeroed siginfo is valid writable storage; WNOWAIT
                // leaves the leader unreaped so its process-group id stays ours.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let waited = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as u32,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if waited != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                if unsafe { info.si_pid() } != 0 {
                    break;
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: the unreaped leader pins the process-group id.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let status = child.wait()?;
            report.exit_code = status.code();
            report.error = if timed_out {
                Some("step exceeded the rehearsal time limit".into())
            } else {
                status
                    .signal()
                    .map(|signal| format!("terminated by signal {signal}"))
            };
        }
    }
    report.seconds = seconds(started);
    report.stdout_tail = file_tail(&out);
    report.stderr_tail = file_tail(&err);
    Ok(report)
}

struct Prepared {
    source: SourceReport,
    locale: String,
}

/// Verify the source is readable and the work directory is safe before any
/// file or process exists. Failing here leaves nothing to clean up.
fn preflight(plan: &Plan, deadline: Instant) -> Result<Prepared> {
    let info = fs::metadata(&plan.work_directory)?;
    // SAFETY: getuid has no preconditions.
    if !pg_core::unredirected(&plan.work_directory)?
        || !info.is_dir()
        || info.uid() != unsafe { libc::getuid() }
        || info.mode() & 0o022 != 0
    {
        return Err(invalid(
            "rehearsal work_directory must be an owned, unredirected directory without group/other write",
        ));
    }
    match &plan.source {
        Source::Dump(path) => {
            check_work_directory(&plan.work_directory, std::slice::from_ref(path))?;
            let toc = pg(
                &plan.package,
                "pg_restore",
                vec!["-l".into(), path.display().to_string()],
                &[],
                deadline,
            )?;
            Ok(Prepared {
                source: SourceReport {
                    kind: "dump",
                    database: toc_header(&toc, "dbname").unwrap_or_default().into(),
                    server_version: toc_header(&toc, "Dumped from database version")
                        .map(Into::into),
                    dump_bytes: Some(dump_bytes(path)?),
                    toc_sha256: Some(codec::digest(toc.as_bytes())),
                },
                locale: plan.locale.clone().unwrap_or_else(|| "C".into()),
            })
        }
        Source::Live(live) => {
            let mut args = vec![
                "-X".into(),
                "-At".into(),
                "-F".into(),
                "|".into(),
                "-h".into(),
                live.host.clone(),
                "-d".into(),
                live.database.clone(),
            ];
            args.extend(
                live.user
                    .iter()
                    .flat_map(|user| ["-U".into(), user.clone()]),
            );
            args.extend(["-w".into(), "-c".into(), "SELECT current_setting('server_version'), datcollate, coalesce((SELECT setting FROM pg_settings WHERE name = 'data_directory'), '') FROM pg_database WHERE datname = current_database()".into()]);
            let row = pg(
                &plan.package,
                "psql",
                args,
                &[("PGCONNECT_TIMEOUT", "10")],
                deadline,
            )?;
            let mut fields = row.trim_end().splitn(3, '|');
            let (version, collation, data) = (
                fields.next().unwrap_or_default(),
                fields.next().unwrap_or_default(),
                fields.next().unwrap_or_default(),
            );
            // The data directory is visible only to privileged readers; when it
            // is, a work directory inside it is refused outright.
            let protected: Vec<PathBuf> = [data, &live.host]
                .into_iter()
                .filter(|path| path.starts_with('/'))
                .map(PathBuf::from)
                .collect();
            check_work_directory(&plan.work_directory, &protected)?;
            Ok(Prepared {
                source: SourceReport {
                    kind: "live",
                    database: live.database.clone(),
                    server_version: Some(version.into()),
                    ..SourceReport::default()
                },
                locale: plan.locale.clone().unwrap_or_else(|| collation.into()),
            })
        }
    }
}

fn execute(
    plan: &Plan,
    prepared: &Prepared,
    run_dir: &Path,
    deadline: Instant,
    receipt: &mut Receipt,
) -> Result<()> {
    let jobs = plan.jobs.unwrap_or(4).to_string();
    let package = plan.package.as_path();
    let socket_dir = run_dir.join("socket");
    let superuser = match &plan.superuser {
        Some(superuser) => superuser.clone(),
        None => process::current_username()?,
    };
    if superuser == plan.owner {
        return Err(invalid(
            "rehearsal owner must not be the cluster superuser account",
        ));
    }
    let cluster = Cluster {
        package,
        socket: socket_dir.display().to_string(),
        user: superuser.clone(),
    };
    if socket_dir
        .join(format!(".s.PGSQL.{PORT}"))
        .as_os_str()
        .len()
        > 100
    {
        return Err(invalid(
            "rehearsal work_directory is too deep for a Unix socket path",
        ));
    }
    // Take the dump first so a failing source costs no cluster.
    let dump = match &plan.source {
        Source::Dump(path) => path.clone(),
        Source::Live(live) => {
            let started = Instant::now();
            let dump = run_dir.join("dump");
            let mut args = vec![
                "-w".into(),
                "-Fd".into(),
                // 31 GB instead of ~77 GB for the pink-raven production copy,
                // in 482 s with four jobs.
                "-Z".into(),
                "zstd:3".into(),
                "-j".into(),
                jobs.clone(),
                "-h".into(),
                live.host.clone(),
                "-d".into(),
                live.database.clone(),
            ];
            args.extend(
                live.user
                    .iter()
                    .flat_map(|user| ["-U".into(), user.clone()]),
            );
            args.extend(["-f".into(), dump.display().to_string()]);
            pg(
                package,
                "pg_dump",
                args,
                &[("PGCONNECT_TIMEOUT", "10")],
                deadline,
            )?;
            receipt.dump_seconds = Some(seconds(started));
            receipt.source.dump_bytes = Some(dump_bytes(&dump)?);
            dump
        }
    };
    let toc = pg(
        package,
        "pg_restore",
        vec!["-l".into(), dump.display().to_string()],
        &[],
        deadline,
    )?;
    if receipt.source.toc_sha256.is_none() {
        receipt.source.toc_sha256 = Some(codec::digest(toc.as_bytes()));
    }

    // Every extension the source has, plus the ones the rehearsed migration
    // needs, exists before the restore and is created by the superuser: the
    // owner may not create untrusted ones, and ACLs may reference their objects.
    let extensions: Vec<String> = toc_extensions(&toc)
        .into_iter()
        .chain(plan.superuser_extensions.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // Production grants name roles a disposable cluster lacks. NOLOGIN stand-ins
    // keep the restored privileges exact instead of dropping the ACLs.
    let acl_list = run_dir.join("acl.list");
    durable::atomic_write(&acl_list, acl_toc(&toc).as_bytes())?;
    let acl_sql = pg(
        package,
        "pg_restore",
        vec![
            "-L".into(),
            acl_list.display().to_string(),
            "-f".into(),
            "-".into(),
            dump.display().to_string(),
        ],
        &[],
        deadline,
    )?;
    let standins: Vec<String> = acl_roles(&acl_sql)
        .into_iter()
        .chain(plan.grantee_roles.iter().cloned())
        .filter(|role| *role != plan.owner && *role != cluster.user)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    receipt.extensions = extensions.clone();
    receipt.standin_roles = standins.clone();

    fs::DirBuilder::new().mode(0o700).create(&socket_dir)?;
    postgres_drill::start_cluster(
        package,
        run_dir,
        Some(&superuser),
        &SERVER_SETTINGS,
        deadline,
    )?;
    let (user, db) = (quote_ident(&plan.owner), quote_ident(&plan.database));
    cluster.sql(
        &cluster.user,
        "postgres",
        &format!("CREATE ROLE {user} LOGIN NOSUPERUSER"),
        &[],
        deadline,
    )?;
    for role in &standins {
        cluster.sql(
            &cluster.user,
            "postgres",
            &format!("CREATE ROLE {} NOLOGIN", quote_ident(role)),
            &[],
            deadline,
        )?;
    }
    cluster.sql(
        &cluster.user,
        "postgres",
        &format!(
            "CREATE DATABASE {db} OWNER {user} TEMPLATE template0 LOCALE {} ENCODING 'UTF8'",
            quote_literal(&prepared.locale)
        ),
        &[],
        deadline,
    )?;
    // ponytail: extensions are created in the default schema; a source that
    // keeps one elsewhere fails the strict restore loudly rather than silently.
    for extension in &extensions {
        cluster.sql(
            &cluster.user,
            &plan.database,
            &format!(
                "CREATE EXTENSION IF NOT EXISTS {} CASCADE",
                quote_ident(extension)
            ),
            &[],
            deadline,
        )?;
    }
    // Extension member tables (e.g. postgis spatial_ref_sys) belong to the
    // extension, which CREATE EXTENSION already populated; restoring their data
    // as the owner is denied. Production edits to such tables are not carried
    // into the rehearsal copy.
    let member_tables = cluster.sql(
        &cluster.user,
        &plan.database,
        "SELECT n.nspname || ' ' || c.relname FROM pg_depend d JOIN pg_class c ON c.oid = d.objid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE d.classid = 'pg_class'::regclass AND d.deptype = 'e' AND c.relkind IN ('r', 'p')",
        &[],
        deadline,
    )?;
    let list = run_dir.join("restore.list");
    durable::atomic_write(
        &list,
        filter_toc(&toc, &extensions, &member_tables).as_bytes(),
    )?;
    // Strict: pg_restore exits non-zero on any error, which fails the rehearsal.
    let started = Instant::now();
    let mut args = vec![
        "--exit-on-error".into(),
        "--no-owner".into(),
        format!("--role={}", plan.owner),
        "-j".into(),
        jobs,
        "-L".into(),
        list.display().to_string(),
    ];
    args.extend(cluster.endpoint(&cluster.user, &plan.database));
    args.push(dump.display().to_string());
    pg(package, "pg_restore", args, &[], deadline)?;
    // Default privileges as the superuser, without --role, as production set them.
    let default_acl = run_dir.join("default-acl.list");
    durable::atomic_write(&default_acl, default_acl_toc(&toc).as_bytes())?;
    let mut args = vec![
        "--exit-on-error".into(),
        "--no-owner".into(),
        "-L".into(),
        default_acl.display().to_string(),
    ];
    args.extend(cluster.endpoint(&cluster.user, &plan.database));
    args.push(dump.display().to_string());
    pg(package, "pg_restore", args, &[], deadline)?;
    receipt.restore_seconds = Some(seconds(started));

    let logs = run_dir.join("logs");
    let (home, cwd) = (run_dir.join("home"), run_dir.join("cwd"));
    for dir in [&logs, &home, &cwd] {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
    }
    let mut base: BTreeMap<String, String> = ["LANG", "LOCALE_ARCHIVE", "TZ"]
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect();
    let path = std::env::var("PATH").unwrap_or_default();
    base.insert(
        "PATH".into(),
        format!("{}:{path}", package.join("bin").display()),
    );
    base.insert("HOME".into(), home.display().to_string());
    base.insert(
        "DATABASE_URL".into(),
        format!(
            "postgres:///{}?host={}&port={PORT}&user={}",
            percent(&plan.database),
            percent(&cluster.socket),
            percent(&plan.owner)
        ),
    );
    for (key, value) in [
        ("PGHOST", cluster.socket.as_str()),
        ("PGPORT", PORT),
        ("PGUSER", &plan.owner),
        ("PGDATABASE", &plan.database),
    ] {
        base.insert(key.into(), value.into());
    }
    for (index, step) in plan.steps.iter().enumerate() {
        let mut env = base.clone();
        env.extend(step.env.clone());
        let report = run_step(step, &env, &cwd, &logs, index, deadline)?;
        let failed = report.error.is_some() || report.exit_code != Some(step.expect_exit);
        let message = format!(
            "step {} ended with {} (expected exit {}){}",
            step.name,
            report
                .exit_code
                .map_or("no exit code".into(), |c| format!("exit {c}")),
            step.expect_exit,
            report
                .error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        );
        receipt.steps.push(report);
        if failed {
            return Err(invalid(message));
        }
    }
    // ponytail: read-only is a session default a probe could override; the
    // cluster is disposable, so this guards mistakes rather than hostility.
    let mut failure = None;
    for probe in &plan.probes {
        let result = cluster.sql(
            &plan.owner,
            &plan.database,
            &probe.sql,
            &[("PGOPTIONS", "-c default_transaction_read_only=on")],
            deadline,
        );
        let (result, error) = match result {
            Ok(text) => (
                Some(text.trim_end().chars().take(PROBE_BYTES).collect()),
                None,
            ),
            Err(error) => (None, Some(error.to_string())),
        };
        failure = failure.or_else(|| {
            error
                .as_ref()
                .map(|e| format!("probe {} failed: {e}", probe.name))
        });
        receipt.probes.push(ProbeReport {
            name: probe.name.clone(),
            sql: probe.sql.clone(),
            result,
            error,
        });
    }
    failure.map_or(Ok(()), |message| Err(invalid(message)))
}

/// Stop the disposable cluster and, unless `keep`, remove its directory. A
/// directory whose cluster could not be stopped is never removed.
fn cleanup(run_dir: &Path, keep: bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(CLEANUP_SECONDS);
    if let Err(error) =
        postgres_drill::cleanup(&run_dir.join("cluster"), &run_dir.join("socket"), deadline)
    {
        return format!("cluster stop failed, work directory retained: {error}");
    }
    if keep {
        return format!(
            "cluster stopped, work directory kept at {}",
            run_dir.display()
        );
    }
    match fs::remove_dir_all(run_dir) {
        Ok(()) => "removed".into(),
        Err(error) => format!("cluster stopped, work directory removal failed: {error}"),
    }
}

/// Rehearse `plan_bytes`, publishing the receipt under `receipt_dir`. A plan or
/// source rejected by preflight returns an error without a receipt; anything
/// after that is recorded in the receipt, whose `status` tells the verdict.
pub fn rehearse(plan_bytes: &[u8], receipt_dir: &Path) -> Result<Outcome> {
    let started = Instant::now();
    let plan = parse_plan(plan_bytes)?;
    let timeout = postgres_drill::timeout_seconds(Some(
        plan.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
    ))?;
    let deadline = started + Duration::from_secs(timeout);
    let prepared = preflight(&plan, deadline)?;
    let now = chrono::Utc::now();
    let run_dir = plan
        .work_directory
        .join(format!("r-{}-{}", now.timestamp(), std::process::id()));
    let mut receipt = Receipt {
        schema: RECEIPT_SCHEMA,
        status: "failed",
        plan_sha256: codec::digest(plan_bytes),
        started_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        package: plan.package.display().to_string(),
        owner: plan.owner.clone(),
        database: plan.database.clone(),
        locale: prepared.locale.clone(),
        source: prepared.source.clone(),
        extensions: vec![],
        standin_roles: vec![],
        dump_seconds: None,
        restore_seconds: None,
        steps: vec![],
        probes: vec![],
        failure: None,
        cleanup: String::new(),
        total_seconds: 0.0,
    };
    fs::DirBuilder::new().mode(0o700).create(&run_dir)?;
    match execute(&plan, &prepared, &run_dir, deadline, &mut receipt) {
        Ok(()) => receipt.status = "passed",
        Err(error) => receipt.failure = Some(error.to_string()),
    }
    receipt.cleanup = cleanup(&run_dir, plan.keep);
    receipt.total_seconds = seconds(started);

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(receipt_dir)?;
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut path = receipt_dir.join(format!("{stamp}.json"));
    if path.exists() {
        path = receipt_dir.join(format!("{stamp}-{}.json", std::process::id()));
    }
    durable::write_json(&path, &serde_json::to_value(&receipt)?)?;
    Ok(Outcome {
        receipt_sha256: codec::file_digest(&path)?,
        receipt: path,
        passed: receipt.status == "passed",
    })
}

/// Read a plan file, refusing anything unreasonably large or not a regular file.
pub fn read_plan(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    durable::open_regular(path, false)?
        .take(PLAN_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > PLAN_LIMIT {
        return Err(invalid("rehearsal plan is too large"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(extra: &str) -> String {
        format!(
            r#"{{"schema":"{PLAN_SCHEMA}","package":"/","source":{{"dump":"/d"}},"owner":"o","database":"d","work_directory":"/w"{extra}}}"#
        )
    }

    #[test]
    fn toc_filter_drops_only_listed_extensions_and_their_comments() {
        let toc = "; header\n3; 3079 1 EXTENSION - postgis o\n4; 0 0 COMMENT - EXTENSION postgis \n5; 3079 2 EXTENSION - pgcrypto o\n6; 0 0 COMMENT - EXTENSION pgcrypto \n7; 1259 3 TABLE public postgis_notes o\n8; 0 1 TABLE DATA public spatial_ref_sys o\n9; 0 2 TABLE DATA public records o\n10; 826 0 DEFAULT ACL public DEFAULT PRIVILEGES FOR TABLES o\n";
        let kept = filter_toc(toc, &["postgis".into()], "public spatial_ref_sys");
        assert!(!kept.contains("postgis o") && !kept.contains("COMMENT - EXTENSION postgis"));
        assert!(kept.contains("pgcrypto o") && kept.contains("COMMENT - EXTENSION pgcrypto"));
        assert!(kept.contains("TABLE public postgis_notes"));
        assert!(!kept.contains("spatial_ref_sys") && kept.contains("TABLE DATA public records"));
        // Default ACLs wait for the superuser pass.
        assert!(!kept.contains("DEFAULT ACL"));
        assert_eq!(default_acl_toc(toc).lines().count(), 1);
    }

    #[test]
    fn toc_yields_extensions_and_acl_entries_only() {
        let toc = "; Archive created\n3; 3079 1 EXTENSION - postgis o\n5; 3079 2 EXTENSION - uuid-ossp o\n6; 1259 3 TABLE public EXTENSION o\n7; 0 0 ACL public TABLE records o\n8; 826 0 DEFAULT ACL public DEFAULT PRIVILEGES FOR TABLES o\n9; 1259 4 TABLE public acl o\n";
        assert_eq!(
            toc_extensions(toc),
            BTreeSet::from(["postgis".into(), "uuid-ossp".into()])
        );
        let acl = acl_toc(toc);
        assert_eq!(acl.lines().count(), 2);
        assert!(acl.contains("ACL public TABLE records") && acl.contains("DEFAULT ACL"));
    }

    #[test]
    fn acl_sql_yields_every_grantee_and_default_privilege_owner() {
        let sql = "SET search_path = public;\nREVOKE ALL ON TABLE public.t FROM PUBLIC;\nGRANT SELECT ON TABLE public.t TO can;\nGRANT SELECT, INSERT ON TABLE public.u TO \"Mixed \"\"Role\", pink_raven_etl WITH GRANT OPTION;\nALTER DEFAULT PRIVILEGES FOR ROLE pink_raven IN SCHEMA public GRANT SELECT ON TABLES TO can;\nREVOKE ALL ON TABLE public.v FROM \"old\";\nSELECT 1; -- ignored TO nobody\n";
        assert_eq!(
            acl_roles(sql),
            BTreeSet::from([
                "can".into(),
                "Mixed \"Role".into(),
                "pink_raven_etl".into(),
                "pink_raven".into(),
                "old".into()
            ])
        );
        assert!(acl_roles("GRANT USAGE ON SCHEMA s TO PUBLIC, CURRENT_USER;").is_empty());
    }

    #[test]
    fn work_directory_overlap_is_refused_in_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        fs::create_dir_all(data.join("work")).unwrap();
        assert!(check_work_directory(&data.join("work"), std::slice::from_ref(&data)).is_err());
        assert!(check_work_directory(root.path(), std::slice::from_ref(&data)).is_err());
        fs::create_dir(root.path().join("other")).unwrap();
        check_work_directory(&root.path().join("other"), &[data]).unwrap();
    }

    #[test]
    fn plan_rejects_unknown_fields_and_wrong_or_missing_schema() {
        // Syntax and field checks run before the package is inspected.
        let error = |text: &str| parse_plan(text.as_bytes()).unwrap_err().to_string();
        assert!(error(&plan(r#","surprise":1"#)).contains("unknown field"));
        assert!(
            error(&plan("").replace(PLAN_SCHEMA, "harbor-db.rehearsal-plan.v2"))
                .contains("schema must be")
        );
        assert!(
            error(&plan("").replace(&format!(r#""schema":"{PLAN_SCHEMA}","#), ""))
                .contains("missing field")
        );
        assert!(
            error(&plan(
                r#","steps":[{"name":"s","kind":"migrate","argv":["x"],"nope":1}]"#
            ))
            .contains("unknown field")
        );
        assert!(
            error(&plan("").replace(
                r#"{"dump":"/d"}"#,
                r#"{"live":{"host":"h","database":"d","extra":1}}"#
            ))
            .contains("unknown field")
        );
        assert!(
            error(&plan("").replace(
                r#""dump":"/d""#,
                r#""dump":"/d","live":{"host":"h","database":"d"}"#
            ))
            .contains("expected")
        );
    }

    #[test]
    fn plan_rejects_an_owner_that_is_the_bootstrap_superuser() {
        let package = tempfile::tempdir().unwrap();
        fs::create_dir(package.path().join("bin")).unwrap();
        for program in ["initdb", "pg_ctl", "pg_restore", "pg_dump", "psql"] {
            fs::write(package.path().join("bin").join(program), "").unwrap();
        }
        let text = |superuser: &str| {
            plan(&format!(r#","superuser":"{superuser}""#)).replace(
                r#""package":"/""#,
                &format!(r#""package":"{}""#, package.path().display()),
            )
        };
        let rejected = parse_plan(text("o").as_bytes());
        assert!(
            rejected
                .unwrap_err()
                .to_string()
                .contains("must not be the cluster superuser"),
            "the owner cannot be the disposable cluster's bootstrap superuser"
        );
        parse_plan(text("bootstrap").as_bytes()).unwrap();
    }

    #[test]
    fn steps_cannot_retarget_the_connection() {
        let package = tempfile::tempdir().unwrap();
        fs::create_dir(package.path().join("bin")).unwrap();
        for program in ["initdb", "pg_ctl", "pg_restore", "pg_dump", "psql"] {
            fs::write(package.path().join("bin").join(program), "").unwrap();
        }
        let text = |env: &str| {
            plan(&format!(
                r#","steps":[{{"name":"s","kind":"check","argv":["true"],"env":{env}}}]"#
            ))
            .replace(
                r#""package":"/""#,
                &format!(r#""package":"{}""#, package.path().display()),
            )
        };
        parse_plan(text(r#"{"RUST_LOG":"info"}"#).as_bytes()).unwrap();
        for key in ["DATABASE_URL", "PGHOST", "PGOPTIONS"] {
            let rejected = parse_plan(text(&format!(r#"{{"{key}":"x"}}"#)).as_bytes());
            assert!(
                rejected.unwrap_err().to_string().contains("cannot set"),
                "{key}"
            );
        }
    }
}
