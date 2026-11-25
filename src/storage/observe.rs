//! One read-only PostgreSQL activity snapshot and its pure finding transitions.
use super::{Result, durable, invalid, process};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const SNAPSHOT_SQL: &str = r#"
BEGIN TRANSACTION READ ONLY;
SELECT pg_catalog.jsonb_build_object(
    'observed_at', pg_catalog.to_char(pg_catalog.now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
    'postmaster_start_time', pg_catalog.to_char(pg_catalog.pg_postmaster_start_time() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
    'track_activities', pg_catalog.current_setting('track_activities') = 'on',
    'backends', coalesce((
        SELECT pg_catalog.jsonb_agg(pg_catalog.to_jsonb(backend) ORDER BY backend.pid)
        FROM (
            SELECT a.pid,
                pg_catalog.to_char(a.backend_start AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS backend_start,
                a.backend_type, a.datname, a.usename, a.application_name, a.query_id,
                a.backend_xmin::text AS backend_xmin,
                pg_catalog.to_char(a.xact_start AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS xact_start,
                pg_catalog.to_char(a.query_start AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS query_start,
                pg_catalog.to_char(a.state_change AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS state_change,
                a.state, a.wait_event_type, a.wait_event,
                (
                    -- Keep the query text server-side. Walk past leading comments and
                    -- nested block delimiters, stopping after 256 steps or at a token.
                    WITH RECURSIVE prefix(rest, depth, steps) AS (
                        SELECT pg_catalog.ltrim(a.query, E' \t\n\r\f\013'), 0, 0
                        UNION ALL
                        SELECT pg_catalog.ltrim(pg_catalog.substr(p.rest, token.pos + 2), E' \t\n\r\f\013'),
                            p.depth + token.delta, p.steps + 1
                        FROM prefix AS p
                        CROSS JOIN LATERAL (
                            SELECT pg_catalog.left(p.rest, 2) AS start,
                                pg_catalog.strpos(p.rest, '/*') AS opens,
                                pg_catalog.strpos(p.rest, '*/') AS closes
                        ) AS delimiters
                        CROSS JOIN LATERAL (
                            SELECT CASE
                                WHEN p.depth > 0 AND delimiters.closes > 0
                                    AND (delimiters.opens = 0 OR delimiters.closes < delimiters.opens)
                                    THEN delimiters.closes
                                WHEN p.depth > 0 THEN delimiters.opens
                                WHEN delimiters.start = '/*' THEN 1
                                WHEN delimiters.start = '--' THEN least(
                                    nullif(pg_catalog.strpos(pg_catalog.substr(p.rest, 3), E'\n'), 0),
                                    nullif(pg_catalog.strpos(pg_catalog.substr(p.rest, 3), E'\r'), 0)
                                )
                            END AS pos,
                            CASE
                                WHEN p.depth > 0 AND delimiters.closes > 0
                                    AND (delimiters.opens = 0 OR delimiters.closes < delimiters.opens) THEN -1
                                WHEN p.depth > 0 OR delimiters.start = '/*' THEN 1
                                ELSE 0
                            END AS delta
                        ) AS token
                        WHERE p.steps < 256 AND token.pos > 0
                    )
                    SELECT EXISTS (
                        SELECT 1 FROM prefix WHERE depth = 0 AND rest ~* $commit$^(COMMIT|END)\y$commit$
                    )
                ) AS is_commit,
                pg_catalog.pg_blocking_pids(a.pid) AS blocking_pids
            FROM pg_catalog.pg_stat_activity AS a
            WHERE a.pid <> pg_catalog.pg_backend_pid()
        ) AS backend
    ), '[]'::jsonb)
)
WHERE pg_catalog.pg_has_role(current_user, 'pg_read_all_stats', 'USAGE');
COMMIT;
"#;

const SNAPSHOT_LIMIT: usize = 8 * 1024 * 1024;
pub const MAX_PREVIOUS_BYTES: usize = 32 * 1024 * 1024;
const MAX_CHAIN_PIDS: usize = 64;
const MAX_BLOCKING_HEADS: usize = 16;

pub struct Connection {
    pub psql: PathBuf,
    pub socket_dir: PathBuf,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub connect_timeout_seconds: u64,
    pub statement_timeout_ms: u64,
    pub process_timeout_seconds: u64,
}

pub fn capture(connection: &Connection) -> Result<ObservationSnapshot> {
    let socket = connection
        .socket_dir
        .to_str()
        .filter(|s| s.starts_with('/') && !s.contains(','))
        .ok_or_else(|| invalid("--socket-dir must name one absolute Unix socket directory"))?;
    let psql = connection
        .psql
        .to_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("--psql must name a UTF-8 executable path"))?;
    if connection.port == 0
        || connection.database.is_empty()
        || connection.user.is_empty()
        || !(1..=i32::MAX as u64).contains(&connection.connect_timeout_seconds)
        || !(1..=i32::MAX as u64).contains(&connection.statement_timeout_ms)
        || connection.process_timeout_seconds == 0
    {
        return Err(invalid(
            "invalid observer connection parameters or timeouts",
        ));
    }
    let mut command = process::CommandSpec::new(
        [
            psql,
            "-X",
            "-w",
            "-q",
            "-A",
            "-t",
            "-v",
            "ON_ERROR_STOP=1",
            "-f",
            "-",
        ]
        .map(str::to_owned)
        .into(),
    );
    // Pass no inherited PG settings, service definitions, or credentials.
    // In particular, PGDATABASE is a literal default; psql -d expands conninfo.
    let ms = connection.statement_timeout_ms;
    let mut environment = BTreeMap::from([
        ("PGHOST".into(), socket.into()),
        ("PGPORT".into(), connection.port.to_string()),
        ("PGDATABASE".into(), connection.database.clone()),
        ("PGUSER".into(), connection.user.clone()),
        (
            "PGCONNECT_TIMEOUT".into(),
            connection.connect_timeout_seconds.to_string(),
        ),
        ("PGAPPNAME".into(), "harbor-db-observer".into()),
        ("PGCLIENTENCODING".into(), "UTF8".into()),
        ("PGPASSFILE".into(), "/dev/null".into()),
        (
            "PGOPTIONS".into(),
            format!(
                "-c default_transaction_read_only=on -c statement_timeout={ms} -c lock_timeout={ms} -c search_path=pg_catalog"
            ),
        ),
    ]);
    if let Ok(path) = std::env::var("PATH") {
        environment.insert("PATH".into(), path);
    }
    command.environment = Some(environment);
    command.timeout = Duration::from_secs(connection.process_timeout_seconds);
    command.maximum_output = SNAPSHOT_LIMIT;
    command.capture_stderr = true; // Bound diagnostics too; never print their contents.
    command.input = Some(SNAPSHOT_SQL.as_bytes().to_vec());
    let output = process::execute(&command)?;
    serde_json::from_slice(&output).map_err(|_| invalid("invalid PostgreSQL observer snapshot"))
}

pub fn read_previous(path: &Path) -> Result<ObserverState> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Report { state: ObserverState },
        State(ObserverState),
    }
    let mut bytes = Vec::new();
    durable::open_regular(path, false)?
        .take((MAX_PREVIOUS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PREVIOUS_BYTES {
        return Err(invalid(
            "previous PostgreSQL observer state exceeded its size limit",
        ));
    }
    match serde_json::from_slice(&bytes)
        .map_err(|_| invalid("invalid previous PostgreSQL observer state"))?
    {
        Input::Report { state } | Input::State(state) => Ok(state),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(DateTime<Utc>);

impl TryFrom<String> for Timestamp {
    type Error = chrono::ParseError;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        DateTime::parse_from_rfc3339(&value).map(|t| Self(t.to_utc()))
    }
}

impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

impl Timestamp {
    fn over(self, start: Self, limit: u64) -> Option<u64> {
        let age = (self.0 - start.0).to_std().ok()?;
        (age > Duration::from_secs(limit)).then_some(age.as_secs())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationSnapshot {
    pub observed_at: Timestamp,
    pub postmaster_start_time: Timestamp,
    pub track_activities: bool,
    pub backends: Vec<Backend>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub pid: i32,
    pub backend_start: Option<Timestamp>,
    pub backend_type: Option<String>,
    pub datname: Option<String>,
    pub usename: Option<String>,
    pub application_name: Option<String>,
    pub query_id: Option<i64>,
    pub backend_xmin: Option<String>,
    pub xact_start: Option<Timestamp>,
    pub query_start: Option<Timestamp>,
    pub state_change: Option<Timestamp>,
    pub state: Option<String>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub is_commit: bool,
    pub blocking_pids: Vec<i32>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Subject {
    Backend {
        pid: i32,
        backend_start: Timestamp,
        xact_start: Option<Timestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_change: Option<Timestamp>,
    },
    Observer,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    LongTransaction,
    CommitPhase,
    IdleInTransaction,
    LockWait,
    HorizonPinned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub rule: Rule,
    pub subject: Subject,
    pub postmaster_start_time: Timestamp,
    pub since: Timestamp,
    pub last_seen: Timestamp,
    pub age_seconds: u64,
    pub query_id: Option<i64>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub hint: Option<String>,
    pub blocking_heads: Vec<BlockingHead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_head_count: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockingHead {
    pub pid: i32,
    pub backend_start: Option<Timestamp>,
    pub usename: Option<String>,
    pub application_name: Option<String>,
    pub state: Option<String>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub query_id: Option<i64>,
    pub chain_pids: Vec<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_len: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionKind {
    Ended,
    Resolved,
    Opened,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub finding: Finding,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StreakMarker {
    Lock(Timestamp),
    Xmin(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Streak {
    subject: Subject,
    marker: StreakMarker,
    first_seen_at: Timestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverState {
    pub postmaster_start_time: Timestamp,
    pub observed_at: Timestamp,
    pub open_findings: Vec<Finding>,
    streaks: Vec<Streak>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationReport {
    pub snapshot: ObservationSnapshot,
    pub state: ObserverState,
    pub open_findings: Vec<Finding>,
    pub transitions: Vec<Transition>,
}

pub struct ObserveThresholds {
    pub default_long_transaction_seconds: u64,
    pub label_budgets: BTreeMap<String, u64>,
    pub commit_phase_seconds: u64,
    pub idle_in_transaction_seconds: u64,
    pub lock_wait_seconds: u64,
    pub horizon_pinned_seconds: u64,
}

impl Default for ObserveThresholds {
    fn default() -> Self {
        Self {
            default_long_transaction_seconds: 900,
            label_budgets: BTreeMap::new(),
            commit_phase_seconds: 300,
            idle_in_transaction_seconds: 300,
            lock_wait_seconds: 60,
            horizon_pinned_seconds: 3600,
        }
    }
}

fn blocking_heads(
    waiter: &Backend,
    backends: &BTreeMap<i32, &Backend>,
) -> (Vec<BlockingHead>, Option<usize>) {
    let mut queue = VecDeque::from([waiter.pid]);
    let mut parent = BTreeMap::from([(waiter.pid, (waiter.pid, 1))]);
    let mut heads = BTreeSet::new();
    let mut total_heads = 0;
    // Sorted breadth-first traversal retains one shortest chain per head.
    while let Some(pid) = queue.pop_front() {
        let backend = backends.get(&pid).copied();
        let blockers: BTreeSet<_> = backend
            .iter()
            .flat_map(|b| b.blocking_pids.iter().copied())
            .collect();
        if blockers.is_empty() && pid != waiter.pid {
            total_heads += 1;
            heads.insert(pid);
            if heads.len() > MAX_BLOCKING_HEADS {
                heads.pop_last();
            }
        }
        let child_len = parent[&pid].1 + 1;
        for blocker in blockers {
            if let std::collections::btree_map::Entry::Vacant(entry) = parent.entry(blocker) {
                entry.insert((pid, child_len));
                queue.push_back(blocker);
            }
        }
    }
    let evidence = heads
        .into_iter()
        .map(|pid| {
            let len = parent[&pid].1;
            // Keep the end of the chain nearest the blocking head.
            let mut step = pid;
            let mut chain_pids = Vec::with_capacity(len.min(MAX_CHAIN_PIDS));
            for _ in 0..len.min(MAX_CHAIN_PIDS) {
                chain_pids.push(step);
                step = parent[&step].0;
            }
            chain_pids.reverse();
            // A disappearing backend (or prepared transaction, pid 0) has no activity row.
            let backend = backends.get(&pid);
            BlockingHead {
                pid,
                backend_start: backend.and_then(|b| b.backend_start),
                usename: backend.and_then(|b| b.usename.clone()),
                application_name: backend.and_then(|b| b.application_name.clone()),
                state: backend.and_then(|b| b.state.clone()),
                wait_event_type: backend.and_then(|b| b.wait_event_type.clone()),
                wait_event: backend.and_then(|b| b.wait_event.clone()),
                query_id: backend.and_then(|b| b.query_id),
                chain_pids,
                chain_len: (len > MAX_CHAIN_PIDS).then_some(len),
            }
        })
        .collect();
    (
        evidence,
        (total_heads > MAX_BLOCKING_HEADS).then_some(total_heads),
    )
}

pub fn evaluate(
    previous_state: Option<&ObserverState>,
    current_snapshot: &ObservationSnapshot,
    thresholds: &ObserveThresholds,
) -> Result<ObservationReport> {
    let now = current_snapshot.observed_at;
    let epoch = current_snapshot.postmaster_start_time;
    let same_epoch = previous_state.filter(|p| p.postmaster_start_time == epoch);
    if !current_snapshot.track_activities
        || current_snapshot
            .backends
            .iter()
            .any(|backend| backend.state.as_deref() == Some("disabled"))
    {
        return Err(invalid("PostgreSQL activity tracking is disabled"));
    }
    if same_epoch.is_some_and(|p| now < p.observed_at) {
        return Err(invalid("PostgreSQL observer clock moved backwards"));
    }
    let mut previously_open: BTreeMap<_, _> = same_epoch
        .into_iter()
        .flat_map(|p| &p.open_findings)
        .map(|f| ((f.rule, f.subject.clone()), f))
        .collect();
    let previous_streaks: BTreeMap<_, _> = same_epoch
        .into_iter()
        .flat_map(|p| &p.streaks)
        .map(|s| ((&s.subject, &s.marker), s.first_seen_at))
        .collect();
    let mut state = ObserverState {
        postmaster_start_time: epoch,
        observed_at: now,
        open_findings: vec![],
        streaks: vec![],
    };
    let mut transitions = vec![];
    if let Some(old) = previous_state.filter(|p| p.postmaster_start_time != epoch) {
        transitions.extend(old.open_findings.iter().cloned().map(|finding| Transition {
            kind: TransitionKind::Ended,
            finding,
        }));
    }
    let backends: BTreeMap<_, _> = current_snapshot
        .backends
        .iter()
        .map(|b| (b.pid, b))
        .collect();
    for backend in backends.values() {
        let Some(backend_start) = backend.backend_start else {
            continue;
        };
        let subject = Subject::Backend {
            pid: backend.pid,
            backend_start,
            xact_start: backend.xact_start,
            state_change: None,
        };
        let mut check = |rule, age| {
            let Some(age_seconds) = age else { return };
            let mut subject = subject.clone();
            if rule == Rule::IdleInTransaction
                && let Subject::Backend {
                    xact_start: None,
                    state_change,
                    ..
                } = &mut subject
            {
                *state_change = backend.state_change;
            }
            let prior = previously_open.remove(&(rule, subject.clone()));
            let (blocking_heads, blocking_head_count) = if rule == Rule::LockWait {
                blocking_heads(backend, &backends)
            } else {
                (vec![], None)
            };
            let finding = Finding {
                rule,
                subject,
                postmaster_start_time: epoch,
                since: prior.map_or(now, |f| f.since),
                last_seen: now,
                age_seconds,
                query_id: backend.query_id,
                wait_event_type: backend.wait_event_type.clone(),
                wait_event: backend.wait_event.clone(),
                hint: (rule == Rule::CommitPhase).then(|| "deferred triggers".into()),
                blocking_heads,
                blocking_head_count,
            };
            if prior.is_none() {
                transitions.push(Transition {
                    kind: TransitionKind::Opened,
                    finding: finding.clone(),
                });
            }
            state.open_findings.push(finding);
        };
        if backend.backend_type.as_deref() == Some("client backend") {
            let budget = backend
                .application_name
                .as_ref()
                .and_then(|label| thresholds.label_budgets.get(label))
                .copied()
                .unwrap_or(thresholds.default_long_transaction_seconds);
            check(
                Rule::LongTransaction,
                backend.xact_start.and_then(|t| now.over(t, budget)),
            );
            if backend.state.as_deref() == Some("active")
                && backend.is_commit
                && backend.wait_event_type.is_none()
                && backend.wait_event.is_none()
            {
                check(
                    Rule::CommitPhase,
                    backend
                        .query_start
                        .and_then(|t| now.over(t, thresholds.commit_phase_seconds)),
                );
            }
            if backend
                .state
                .as_deref()
                .is_some_and(|s| s.starts_with("idle in transaction"))
            {
                check(
                    Rule::IdleInTransaction,
                    backend
                        .state_change
                        .and_then(|t| now.over(t, thresholds.idle_in_transaction_seconds)),
                );
            }
        }
        let mut streak = |marker, rule, limit| {
            let first = previous_streaks.get(&(&subject, &marker)).copied();
            check(rule, first.and_then(|t| now.over(t, limit)));
            state.streaks.push(Streak {
                subject: subject.clone(),
                marker,
                first_seen_at: first.unwrap_or(now),
            });
        };
        if backend.wait_event_type.as_deref() == Some("Lock")
            && let Some(start) = backend.query_start
        {
            streak(
                StreakMarker::Lock(start),
                Rule::LockWait,
                thresholds.lock_wait_seconds,
            );
        }
        if let Some(xmin) = &backend.backend_xmin {
            streak(
                StreakMarker::Xmin(xmin.clone()),
                Rule::HorizonPinned,
                thresholds.horizon_pinned_seconds,
            );
        }
    }
    transitions.extend(
        previously_open
            .into_values()
            .cloned()
            .map(|finding| Transition {
                kind: TransitionKind::Resolved,
                finding,
            }),
    );
    state
        .open_findings
        .sort_by(|a, b| (a.rule, &a.subject).cmp(&(b.rule, &b.subject)));
    transitions.sort_by(|a, b| {
        (a.kind, a.finding.rule, &a.finding.subject).cmp(&(
            b.kind,
            b.finding.rule,
            &b.finding.subject,
        ))
    });
    let mut snapshot = current_snapshot.clone();
    snapshot.backends.sort_by_key(|b| b.pid);
    for backend in &mut snapshot.backends {
        backend.blocking_pids.sort_unstable();
    }
    Ok(ObservationReport {
        snapshot,
        open_findings: state.open_findings.clone(),
        state,
        transitions,
    })
}
