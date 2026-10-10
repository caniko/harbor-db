//! Explicit idempotent provisioning for dedicated application databases.
use super::{Result, durable, invalid, process, string};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};
const TABLE: &[&str] = &[
    "DELETE",
    "INSERT",
    "MAINTAIN",
    "REFERENCES",
    "SELECT",
    "TRIGGER",
    "TRUNCATE",
    "UPDATE",
];
const SEQUENCE: &[&str] = &["SELECT", "UPDATE", "USAGE"];
fn identifier(s: &str) -> Result<String> {
    if s.is_empty()
        || s.len() > 63
        || !s
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_lowercase() || (i > 0 && b.is_ascii_digit()))
    {
        return Err(invalid(
            "application SQL identifiers must be lowercase and at most 63 bytes",
        ));
    }
    Ok(format!("\"{s}\""))
}
fn privileges(v: &Value, allowed: &[&str]) -> Result<Vec<String>> {
    let a = v
        .as_array()
        .ok_or_else(|| invalid("invalid application privileges"))?;
    let mut seen = BTreeSet::new();
    a.iter()
        .map(|v| {
            let s = v
                .as_str()
                .ok_or_else(|| invalid("invalid application privileges"))?;
            if !allowed.contains(&s) || !seen.insert(s) {
                return Err(invalid("invalid application privileges"));
            }
            Ok(s.to_owned())
        })
        .collect()
}
pub fn validate(c: &Value) -> Result<Value> {
    let m = c
        .as_object()
        .ok_or_else(|| invalid("invalid provisioning policy"))?;
    if m.keys().any(|k| {
        ![
            "database",
            "owner_role",
            "runtime_role",
            "schema",
            "table_privileges",
            "sequence_privileges",
            "tables",
        ]
        .contains(&k.as_str())
    }) {
        return Err(invalid("unknown application provisioning policy"));
    }
    let mut p = json!({"schema":"public","table_privileges":["SELECT"],"sequence_privileges":["USAGE","SELECT"],"tables":{}});
    for (k, v) in m {
        p[k] = v.clone();
    }
    for k in ["database", "owner_role", "runtime_role", "schema"] {
        identifier(string(&p, k)?)?;
    }
    let db = string(&p, "database")?;
    let schema = string(&p, "schema")?;
    let owner = string(&p, "owner_role")?;
    let runtime = string(&p, "runtime_role")?;
    if ["postgres", "template0", "template1"].contains(&db) || schema.starts_with("pg_") {
        return Err(invalid(
            "application database and schema must not be system resources",
        ));
    }
    if owner == runtime
        || [owner, runtime]
            .iter()
            .any(|r| *r == "postgres" || r.starts_with("pg_"))
    {
        return Err(invalid(
            "application roles must be distinct dedicated non-system roles",
        ));
    }
    privileges(&p["table_privileges"], TABLE)?;
    privileges(&p["sequence_privileges"], SEQUENCE)?;
    for (name, values) in p["tables"]
        .as_object()
        .ok_or_else(|| invalid("table privilege exceptions must be a mapping"))?
    {
        identifier(name)?;
        privileges(values, TABLE)?;
    }
    Ok(p)
}
fn query(e: &Value, db: &str, sql: &str) -> Result<String> {
    identifier(db)?;
    let role = string(e, "control_role")?;
    identifier(role)?;
    let package = Path::new(string(e, "package")?);
    let socket = string(e, "socket_dir")?;
    let port = e
        .get("port")
        .and_then(Value::as_number)
        .ok_or_else(|| invalid("explicit local PostgreSQL endpoint is required"))?
        .to_string();
    if !package.is_absolute()
        || !Path::new(socket).is_absolute()
        || !port.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid("explicit local PostgreSQL endpoint is required"));
    }
    let mut spec = process::CommandSpec::new(vec![
        package.join("bin/psql").to_string_lossy().into_owned(),
        "-X".into(),
        "-w".into(),
        "-qAt".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "-h".into(),
        socket.into(),
        "-p".into(),
        port,
        "-U".into(),
        role.into(),
        "-d".into(),
        db.into(),
        "-f".into(),
        "-".into(),
    ]);
    let mut env: std::collections::BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("PG"))
        .collect();
    env.insert(
        "PGOPTIONS".into(),
        "-c lock_timeout=5000 -c statement_timeout=30000".into(),
    );
    spec.environment = Some(env);
    spec.input = Some(sql.as_bytes().to_vec());
    let out = process::execute(&spec)
        .map_err(|_| invalid("application provisioning SQL failed; diagnostics suppressed"))?;
    Ok(process::text(&out)?.trim().to_owned())
}
pub fn apply(c: &Value, e: &Value) -> Result<()> {
    let _lease = durable::lock(Path::new(string(e, "lock_file")?), false, false)?;
    let p = validate(c)?;
    let db = string(&p, "database")?;
    let o = string(&p, "owner_role")?;
    let r = string(&p, "runtime_role")?;
    let s = string(&p, "schema")?;
    let database = identifier(db)?;
    let owner = identifier(o)?;
    let runtime = identifier(r)?;
    let schema = identifier(s)?;
    if query(
        e,
        "postgres",
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles r ON r.oid=m.member OR r.oid=m.roleid WHERE r.rolname IN ('{o}','{r}'));"
        ),
    )? == "t"
    {
        return Err(invalid(
            "dedicated application roles have unexpected role membership",
        ));
    }
    let unsafe_sql = format!(
        "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname='{db}' AND pg_get_userbyid(datdba)<>'{o}') OR EXISTS (SELECT 1 FROM pg_roles WHERE rolname IN ('{o}','{r}') AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)) OR EXISTS (SELECT 1 FROM pg_shdepend d JOIN pg_roles r ON r.oid=d.refobjid WHERE d.refclassid='pg_authid'::regclass AND d.deptype='o' AND r.rolname IN ('{o}','{r}') AND d.dbid<>COALESCE((SELECT oid FROM pg_database WHERE datname='{db}'),0) AND NOT (d.dbid=0 AND d.classid='pg_database'::regclass AND d.objid=(SELECT oid FROM pg_database WHERE datname='{db}') AND r.rolname='{o}')); "
    );
    if query(e, "postgres", &unsafe_sql)? == "t" {
        return Err(invalid(
            "conflicting application role or database ownership; explicit adoption is required",
        ));
    }
    let mut sql = vec![format!(
        "SELECT pg_advisory_lock(hashtextextended('harbor-db:{db}',0));"
    )];
    for (name, role) in [(o, &owner), (r, &runtime)] {
        sql.push(format!("SELECT 'CREATE ROLE {role} LOGIN' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{name}')\\gexec"));
        sql.push(format!("ALTER ROLE {role} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;"));
    }
    sql.push(format!("SELECT 'CREATE DATABASE {database} OWNER {owner}' WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname='{db}')\\gexec"));
    query(e, "postgres", &sql.join("\n"))?;
    if query(
        e,
        db,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{s}' AND c.relkind IN ('r','p','v','m','f','S') AND pg_get_userbyid(c.relowner)<>'{o}') OR EXISTS (SELECT 1 FROM pg_namespace WHERE nspname='{s}' AND pg_get_userbyid(nspowner)<>'{o}' AND NOT (nspname='public' AND pg_get_userbyid(nspowner)='pg_database_owner'));"
        ),
    )? == "t"
    {
        return Err(invalid(
            "conflicting application object ownership; explicit adoption is required",
        ));
    }
    sql = vec![
        "BEGIN;".into(),
        format!("SELECT pg_advisory_xact_lock(hashtextextended('harbor-db:{db}',0));"),
        format!("REVOKE ALL ON DATABASE {database} FROM PUBLIC, {runtime};"),
        format!("GRANT CONNECT ON DATABASE {database} TO {runtime};"),
        format!("CREATE SCHEMA IF NOT EXISTS {schema} AUTHORIZATION {owner};"),
        format!("ALTER SCHEMA {schema} OWNER TO {owner};"),
        format!("REVOKE ALL ON SCHEMA {schema} FROM PUBLIC, {runtime};"),
        format!("GRANT USAGE ON SCHEMA {schema} TO {runtime};"),
        format!("REVOKE ALL ON ALL TABLES IN SCHEMA {schema} FROM PUBLIC, {runtime};"),
        format!("REVOKE ALL ON ALL SEQUENCES IN SCHEMA {schema} FROM PUBLIC, {runtime};"),
        format!(
            "SELECT format('REVOKE ALL (%I) ON TABLE %I.%I FROM PUBLIC, %I',a.attname,n.nspname,c.relname,'{r}') FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{s}' AND c.relkind IN ('r','p','v','m','f') AND a.attnum>0 AND NOT a.attisdropped\\gexec"
        ),
    ];
    for (kind, key, allowed) in [
        ("TABLES", "table_privileges", TABLE),
        ("SEQUENCES", "sequence_privileges", SEQUENCE),
    ] {
        for scope in [String::new(), format!(" IN SCHEMA {schema}")] {
            sql.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {owner}{scope} REVOKE ALL ON {kind} FROM PUBLIC, {runtime};"));
        }
        let grants = privileges(&p[key], allowed)?;
        if !grants.is_empty() {
            let grants = grants.join(",");
            sql.push(format!(
                "GRANT {grants} ON ALL {kind} IN SCHEMA {schema} TO {runtime};"
            ));
            sql.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA {schema} GRANT {grants} ON {kind} TO {runtime};"));
        }
    }
    for (table, values) in p["tables"].as_object().unwrap() {
        let target = format!("{schema}.{}", identifier(table)?);
        let grants = privileges(values, TABLE)?;
        let mut stmt = format!("REVOKE ALL ON TABLE {target} FROM {runtime};");
        if !grants.is_empty() {
            stmt += &format!("GRANT {} ON TABLE {target} TO {runtime};", grants.join(","));
        }
        sql.push(format!(
            "SELECT '{stmt}' WHERE to_regclass('{target}') IS NOT NULL\\gexec"
        ));
    }
    sql.push("COMMIT;".into());
    query(e, db, &sql.join("\n"))?;
    if !check(&p, e)? {
        return Err(invalid(
            "application provisioning did not converge to the declared privileges",
        ));
    }
    Ok(())
}
pub fn check(c: &Value, e: &Value) -> Result<bool> {
    let p = validate(c)?;
    let db = string(&p, "database")?;
    let o = string(&p, "owner_role")?;
    let r = string(&p, "runtime_role")?;
    let s = string(&p, "schema")?;
    if query(
        e,
        "postgres",
        &format!(
            "SELECT count(*) = 2 AND bool_and(NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls AND rolcanlogin) FROM pg_roles WHERE rolname IN ('{o}','{r}');"
        ),
    )? != "t"
    {
        return Ok(false);
    }
    let mut checks = vec![
        format!("(SELECT pg_get_userbyid(datdba)='{o}' FROM pg_database WHERE datname='{db}')"),
        format!("(SELECT pg_get_userbyid(nspowner)='{o}' FROM pg_namespace WHERE nspname='{s}')"),
        format!("has_schema_privilege('{r}','{s}','USAGE')"),
        format!("NOT has_schema_privilege('{r}','{s}','CREATE')"),
        format!("has_database_privilege('{r}','{db}','CONNECT')"),
        format!("NOT has_database_privilege('{r}','{db}','CREATE')"),
        format!("NOT has_database_privilege('{r}','{db}','TEMP')"),
        format!(
            "NOT EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles r ON r.oid=m.member OR r.oid=m.roleid WHERE r.rolname IN ('{r}','{o}'))"
        ),
        format!(
            "NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{s}' AND c.relkind IN ('r','p','v','m','f','S') AND pg_get_userbyid(c.relowner)<>'{o}')"
        ),
        format!(
            "NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace, LATERAL aclexplode(c.relacl) acl WHERE n.nspname='{s}' AND (acl.grantee=0 OR (acl.grantee=(SELECT oid FROM pg_roles WHERE rolname='{r}') AND acl.is_grantable)))"
        ),
        format!(
            "NOT EXISTS (SELECT 1 FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace, LATERAL aclexplode(a.attacl) acl WHERE n.nspname='{s}' AND acl.grantee IN (0,(SELECT oid FROM pg_roles WHERE rolname='{r}'))) "
        ),
    ];
    let grants = privileges(&p["table_privileges"], TABLE)?;
    for privilege in TABLE {
        let default = grants.iter().any(|v| v == privilege);
        let mut cases = vec![];
        for (table, values) in p["tables"].as_object().unwrap() {
            cases.push(format!(
                "WHEN '{table}' THEN {}",
                privileges(values, TABLE)?.iter().any(|v| v == privilege)
            ));
        }
        let expected = if cases.is_empty() {
            default.to_string()
        } else {
            format!("CASE c.relname {} ELSE {default} END", cases.join(" "))
        };
        checks.push(format!("NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{s}' AND c.relkind IN ('r','p','v','m','f') AND has_table_privilege('{r}',c.oid,'{privilege}') <> ({expected}))"));
    }
    let grants = privileges(&p["sequence_privileges"], SEQUENCE)?;
    for privilege in SEQUENCE {
        let expected = grants.iter().any(|v| v == privilege);
        checks.push(format!("NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE CASE WHEN n.nspname='{s}' AND c.relkind='S' THEN has_sequence_privilege('{r}',c.oid,'{privilege}') <> {expected} ELSE false END)"));
    }
    if query(
        e,
        db,
        &format!(
            "BEGIN READ ONLY; SELECT COALESCE({},false); COMMIT;",
            checks.join(" AND ")
        ),
    )? != "t"
    {
        return Ok(false);
    }
    let defaults: Value = serde_json::from_str(&query(
        e,
        db,
        &format!(
            "BEGIN READ ONLY; SELECT COALESCE(json_agg(json_build_array(d.defaclobjtype,d.defaclnamespace=0,acl.privilege_type,acl.is_grantable,acl.grantee=0)), '[]') FROM pg_default_acl d, LATERAL aclexplode(d.defaclacl) acl WHERE d.defaclrole=(SELECT oid FROM pg_roles WHERE rolname='{o}') AND d.defaclnamespace IN (0,(SELECT oid FROM pg_namespace WHERE nspname='{s}')) AND d.defaclobjtype IN ('r','S') AND acl.grantee IN (0,(SELECT oid FROM pg_roles WHERE rolname='{r}')); COMMIT;"
        ),
    )?)?;
    let mut actual: Vec<String> = defaults
        .as_array()
        .ok_or_else(|| invalid("invalid default privilege receipt"))?
        .iter()
        .map(Value::to_string)
        .collect();
    let mut expected = vec![];
    for (kind, key, allowed) in [
        ("r", "table_privileges", TABLE),
        ("S", "sequence_privileges", SEQUENCE),
    ] {
        for v in privileges(&p[key], allowed)? {
            expected.push(json!([kind, false, v, false, false]).to_string());
        }
    }
    actual.sort();
    expected.sort();
    Ok(actual == expected)
}
