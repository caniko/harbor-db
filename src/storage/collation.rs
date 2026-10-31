//! Detect and repair PostgreSQL collation-version drift (libc/glibc upgrades).
//!
//! Indexes ordered by a collation whose provider version changed may be
//! silently corrupt until rebuilt. Detection is read-only; repair rebuilds the
//! affected indexes online and only then refreshes the recorded database
//! collation version, so an interrupted run leaves the drift visible.
use super::{Result, codec, durable, invalid, pg_core, process};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Options {
    pub socket_dir: PathBuf,
    pub port: u16,
    pub dry_run: bool,
    pub fail_on_drift: bool,
    pub verify: bool,
    pub databases: Vec<String>,
}

const DRIFT_SQL: &str = "SELECT json_build_object('database', datname, 'quoted', format('%I', datname), 'recorded', datcollversion, 'actual', pg_database_collation_actual_version(oid)) FROM pg_database WHERE datallowconn AND datname <> 'template0' ORDER BY datname;";

/// Explicit libc/ICU collations whose recorded version differs from the provider's.
const COLLATION_SQL: &str = "SELECT json_build_object('name', format('%I.%I', n.nspname, c.collname), 'actual', pg_collation_actual_version(c.oid)) FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace WHERE c.collprovider IN ('c', 'i') AND c.collversion IS DISTINCT FROM pg_collation_actual_version(c.oid) ORDER BY n.nspname, c.collname;";

/// Indexes whose ordered key columns (INCLUDE columns excluded) resolve to the
/// drifted database default or to any drifted explicit collation (libc or ICU).
/// Every partial or expression index is included because collations used inside
/// the predicate or expression are not recorded (safe direction).
const INDEX_SQL: &str = "
SELECT json_build_object(
  'qualified', format('%I.%I', n.nspname, c.relname),
  'index', c.relname, 'table', format('%I.%I', tn.nspname, t.relname),
  'am', a.amname, 'bytes', pg_relation_size(c.oid), 'valid', i.indisvalid,
  'catalog', n.nspname IN ('pg_catalog', 'information_schema', 'pg_toast'),
  'exclusion', EXISTS (SELECT 1 FROM pg_constraint x WHERE x.conindid = c.oid AND x.contype = 'x'),
  'leftover', NOT i.indisvalid AND c.relname ~ '_cc(new|old)[0-9]*$')
FROM pg_index i
JOIN pg_class c ON c.oid = i.indexrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class t ON t.oid = i.indrelid
JOIN pg_namespace tn ON tn.oid = t.relnamespace
JOIN pg_am a ON a.oid = c.relam
JOIN pg_database d ON d.datname = current_database()
WHERE c.relkind = 'i' AND (
  EXISTS (
    SELECT 1 FROM unnest(i.indcollation::oid[]) WITH ORDINALITY k(coll, pos)
    WHERE k.pos <= i.indnkeyatts AND k.coll <> 0 AND (
      (k.coll = 100 AND d.datcollversion IS DISTINCT FROM pg_database_collation_actual_version(d.oid))
      OR EXISTS (SELECT 1 FROM pg_collation co WHERE co.oid = k.coll AND co.collprovider IN ('c', 'i') AND co.collversion IS DISTINCT FROM pg_collation_actual_version(co.oid))))
  OR i.indpred IS NOT NULL OR i.indexprs IS NOT NULL)
ORDER BY pg_relation_size(c.oid), c.relname;";

pub struct Collation {
    pub name: String,
    pub actual: String,
}

#[derive(Debug, PartialEq)]
pub struct Item {
    pub qualified: String,
    pub table: String,
    pub am: String,
    pub bytes: u64,
    pub action: &'static str,
}

impl Item {
    /// The single statement executed for this item.
    pub fn statement(&self) -> String {
        match self.action {
            "drop-leftover" => format!("DROP INDEX CONCURRENTLY IF EXISTS {};", self.qualified),
            // ponytail: catalog, exclusion-constraint and invalid indexes cannot
            // be rebuilt concurrently; plain REINDEX blocks writes to the table.
            "reindex-exclusive" => format!("REINDEX INDEX {};", self.qualified),
            _ => format!("REINDEX INDEX CONCURRENTLY {};", self.qualified),
        }
    }
}

/// Parse one JSON object per line of `INDEX_SQL` output into plan items.
pub fn parse_items(output: &str) -> Result<Vec<Item>> {
    let mut items = vec![];
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let v = codec::decode_str(line)?;
        let text = |key: &str| -> Result<String> { Ok(super::string(&v, key)?.to_owned()) };
        let flag = |key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
        let action = if flag("leftover") {
            "drop-leftover"
        } else if flag("catalog") || flag("exclusion") || !flag("valid") {
            "reindex-exclusive"
        } else {
            "reindex-concurrently"
        };
        items.push(Item {
            qualified: text("qualified")?,
            table: text("table")?,
            am: text("am")?,
            bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            action,
        });
    }
    Ok(items)
}

/// Names are passed through PGDATABASE, which libpq does not expand as a
/// connection string, so `=`, spaces and quotes in a name stay literal.
fn psql_spec(
    config: &Value,
    options: &Options,
    database: &str,
    sql: &str,
) -> Result<process::CommandSpec> {
    let mut spec = pg_core::command(
        vec![
            pg_core::path(config, "package")?
                .join("bin/psql")
                .display()
                .to_string(),
            "--no-psqlrc".into(),
            "--no-password".into(),
            format!("--host={}", options.socket_dir.display()),
            format!("--port={}", options.port),
            "--username=postgres".into(),
            "--set=ON_ERROR_STOP=1".into(),
            "--tuples-only".into(),
            "--no-align".into(),
            "--command".into(),
            sql.into(),
        ],
        true,
    );
    // ponytail: one bound for the largest rebuild; per-index scaling if it bites.
    spec.timeout = Duration::from_secs(7 * 24 * 3600);
    spec.maximum_output = 64 << 20;
    spec.capture_stderr = true;
    spec.environment
        .as_mut()
        .unwrap()
        .insert("PGCONNECT_TIMEOUT".into(), "10".into());
    spec.environment
        .as_mut()
        .unwrap()
        .insert("PGDATABASE".into(), database.into());
    Ok(spec)
}

fn psql(config: &Value, options: &Options, database: &str, sql: &str) -> Result<String> {
    let spec = psql_spec(config, options, database, sql)?;
    let output = process::run(&spec)?;
    if !output.status.success() {
        return Err(invalid(format!(
            "psql failed on {database}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn emit(value: Value) {
    println!("{value}");
}

fn state_path(config: &Value, database: &str) -> Result<PathBuf> {
    let directory: &Path = &pg_core::path(config, "state_dir")?;
    // Database names are arbitrary; keep the file name a safe hex token.
    let token: String = database.bytes().map(|b| format!("{b:02x}")).collect();
    Ok(directory.join(format!("collation-refresh-{token}.json")))
}

/// Indexes already rebuilt for this exact actual version, so a rerun resumes.
fn load_done(path: &Path, actual: &str) -> Vec<String> {
    durable::read_json(path)
        .ok()
        .filter(|v| v["actual"] == actual)
        .and_then(|v| serde_json::from_value(v["done"].clone()).ok())
        .unwrap_or_default()
}

fn refresh_database(
    config: &Value,
    options: &Options,
    database: &str,
    quoted: &str,
    actual: &str,
    database_drift: bool,
    collations: &[Collation],
) -> Result<()> {
    let items = parse_items(&psql(config, options, database, INDEX_SQL)?)?;
    for item in &items {
        emit(
            json!({"event": "plan", "database": database, "index": item.qualified,
            "table": item.table, "am": item.am, "bytes": item.bytes, "action": item.action}),
        );
    }
    if options.dry_run {
        return Ok(());
    }
    let state = state_path(config, database)?;
    // The key covers the default and every drifted named collation's actual
    // version, so a further provider change restarts the rebuild.
    let key = format!(
        "{actual};{}",
        collations
            .iter()
            .map(|c| format!("{}={}", c.name, c.actual))
            .collect::<Vec<_>>()
            .join(",")
    );
    let actual = key.as_str();
    let mut done = load_done(&state, actual);
    for item in &items {
        if done.contains(&item.qualified) {
            continue;
        }
        psql(config, options, database, &item.statement())?;
        done.push(item.qualified.clone());
        durable::write_json(&state, &json!({"actual": actual, "done": done}))?;
        emit(
            json!({"event": "done", "database": database, "index": item.qualified,
            "action": item.action}),
        );
    }
    if options.verify {
        verify(config, options, database, &items)?;
    }
    if database_drift {
        psql(
            config,
            options,
            database,
            &format!("ALTER DATABASE {quoted} REFRESH COLLATION VERSION;"),
        )?;
    }
    for collation in collations {
        psql(
            config,
            options,
            database,
            &format!("ALTER COLLATION {} REFRESH VERSION;", collation.name),
        )?;
    }
    let _ = std::fs::remove_file(&state);
    emit(json!({"event": "refreshed", "database": database, "version": actual}));
    Ok(())
}

/// Run `bt_index_check` only where amcheck is already installed; never creates it.
fn verify(config: &Value, options: &Options, database: &str, items: &[Item]) -> Result<()> {
    let installed = psql(
        config,
        options,
        database,
        "SELECT format('%I', n.nspname) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'amcheck';",
    )?;
    let schema = installed.trim();
    if schema.is_empty() {
        emit(json!({"event": "verify-skipped", "database": database,
            "reason": "amcheck is not installed"}));
        return Ok(());
    }
    for item in items
        .iter()
        .filter(|i| i.am == "btree" && i.action != "drop-leftover")
    {
        psql(
            config,
            options,
            database,
            &format!(
                "SELECT {schema}.bt_index_check(index => '{}'::regclass);",
                item.qualified.replace('\'', "''")
            ),
        )?;
        emit(json!({"event": "verified", "database": database, "index": item.qualified}));
    }
    Ok(())
}

pub fn run(config: &Value, options: &Options) -> Result<()> {
    pg_core::validate_config(config)?;
    let drifted = psql(config, options, "postgres", DRIFT_SQL)?;
    let mut failures = vec![];
    let mut drift = 0;
    for line in drifted.lines().filter(|line| !line.trim().is_empty()) {
        let v = codec::decode_str(line)?;
        let name = super::string(&v, "database")?;
        if !options.databases.is_empty() && !options.databases.iter().any(|d| d == name) {
            continue;
        }
        let database_drift = v["recorded"] != v["actual"];
        let mut collations = vec![];
        for line in psql(config, options, name, COLLATION_SQL)?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let c = codec::decode_str(line)?;
            collations.push(Collation {
                name: super::string(&c, "name")?.to_owned(),
                actual: c["actual"].as_str().unwrap_or_default().to_owned(),
            });
        }
        if !database_drift && collations.is_empty() {
            continue;
        }
        drift += 1;
        let actual = v["actual"].as_str().unwrap_or_default();
        emit(
            json!({"event": "drift", "database": name, "recorded": v["recorded"],
            "actual": v["actual"], "collations": collations.iter().map(|c| &c.name).collect::<Vec<_>>()}),
        );
        if let Err(error) = refresh_database(
            config,
            options,
            name,
            super::string(&v, "quoted")?,
            actual,
            database_drift,
            &collations,
        ) {
            emit(json!({"event": "failed", "database": name, "error": error.to_string()}));
            failures.push(name.to_owned());
        }
    }
    if !failures.is_empty() {
        return Err(invalid(format!(
            "collation refresh failed for: {}",
            failures.join(", ")
        )));
    }
    if options.dry_run && options.fail_on_drift && drift > 0 {
        return Err(invalid(format!(
            "collation version drift in {drift} database(s)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(patch: Value) -> Item {
        let mut v = json!({"qualified": "public.\"a b\"", "table": "public.t", "am": "btree",
            "bytes": 5, "valid": true, "catalog": false, "exclusion": false, "leftover": false});
        v.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        parse_items(&v.to_string()).unwrap().remove(0)
    }

    #[test]
    fn plans_classify_each_index_kind() {
        let plain = item(json!({}));
        assert_eq!(
            plain.statement(),
            "REINDEX INDEX CONCURRENTLY public.\"a b\";"
        );
        assert_eq!(item(json!({"exclusion": true})).action, "reindex-exclusive");
        assert_eq!(item(json!({"catalog": true})).action, "reindex-exclusive");
        assert_eq!(item(json!({"valid": false})).action, "reindex-exclusive");
        let left = item(json!({"valid": false, "leftover": true}));
        assert_eq!(
            left.statement(),
            "DROP INDEX CONCURRENTLY IF EXISTS public.\"a b\";"
        );
        assert!(parse_items("").unwrap().is_empty());
    }

    #[test]
    fn database_names_stay_literal_and_index_sql_covers_named_collations() {
        let config = json!({"package": "/nix/store/pg"});
        let options = Options {
            socket_dir: "/run/postgresql".into(),
            port: 5432,
            dry_run: true,
            fail_on_drift: false,
            verify: false,
            databases: vec![],
        };
        let name = "a=b host=evil 'q'";
        let spec = psql_spec(&config, &options, name, "SELECT 1").unwrap();
        assert_eq!(spec.environment.as_ref().unwrap()["PGDATABASE"], name);
        assert!(
            spec.argv
                .iter()
                .all(|a| !a.contains("dbname") && !a.contains(name))
        );
        // Any drifted named collation (libc or ICU) selects its key indexes.
        assert!(
            INDEX_SQL.contains("co.collprovider IN ('c', 'i') AND co.collversion IS DISTINCT FROM")
        );
        assert!(!INDEX_SQL.contains("datcollate NOT IN"));
    }
}
