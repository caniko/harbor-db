"""Explicit, idempotent provisioning of a dedicated application database."""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from .durable import lock

TABLE = {"SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN"}
SEQUENCE = {"USAGE", "SELECT", "UPDATE"}


def identifier(value):
    if not isinstance(value, str) or not re.fullmatch(r"[a-z_][a-z0-9_]{0,62}", value):
        raise ValueError("application SQL identifiers must be lowercase and at most 63 bytes")
    return '"' + value + '"'


def privileges(values, allowed):
    if (not isinstance(values, list) or any(value not in allowed for value in values)
            or len(set(values)) != len(values)):
        raise ValueError("invalid application privileges")
    return values


def validate(config):
    if set(config) - {"database", "owner_role", "runtime_role", "schema", "table_privileges",
                      "sequence_privileges", "tables"}:
        raise ValueError("unknown application provisioning policy")
    result = {"schema": "public", "table_privileges": ["SELECT"],
              "sequence_privileges": ["USAGE", "SELECT"], "tables": {}, **config}
    for key in ("database", "owner_role", "runtime_role", "schema"):
        identifier(result[key])
    if result["database"] in {"postgres", "template0", "template1"} or result["schema"].startswith("pg_"):
        raise ValueError("application database and schema must not be system resources")
    roles = [result["owner_role"], result["runtime_role"]]
    if roles[0] == roles[1] or any(role == "postgres" or role.startswith("pg_") for role in roles):
        raise ValueError("application roles must be distinct dedicated non-system roles")
    privileges(result["table_privileges"], TABLE)
    privileges(result["sequence_privileges"], SEQUENCE)
    if not isinstance(result["tables"], dict):
        raise ValueError("table privilege exceptions must be a mapping")
    for name, values in result["tables"].items():
        identifier(name)
        privileges(values, TABLE)
    return result


def query(endpoint, database, sql):
    identifier(database)
    role = endpoint["control_role"]
    identifier(role)
    package = Path(endpoint["package"])
    socket = Path(endpoint["socket_dir"])
    if not package.is_absolute() or not socket.is_absolute() or type(endpoint["port"]) is not int:
        raise ValueError("explicit local PostgreSQL endpoint is required")
    environment = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    result = subprocess.run([str(package / "bin/psql"), "-X", "-w", "-qAt",
        "-v", "ON_ERROR_STOP=1", "-h", str(socket), "-p", str(endpoint["port"]),
        "-U", role, "-d", database, "-f", "-"], input=sql, text=True,
        capture_output=True, timeout=60, env=environment | {"PGOPTIONS": "-c lock_timeout=5000 -c statement_timeout=30000"})
    if result.returncode:
        raise ValueError("application provisioning SQL failed; diagnostics suppressed")
    return result.stdout.strip()


def role_check(policy):
    roles = [policy["owner_role"], policy["runtime_role"]]
    return ("SELECT count(*) = 2 AND bool_and(NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole "
            "AND NOT rolreplication AND NOT rolbypassrls AND rolcanlogin) FROM pg_roles "
            f"WHERE rolname IN ('{roles[0]}','{roles[1]}');")


def apply(config, endpoint):
    # One persistent cluster-scoped lock covers role creation and the database
    # transaction, including the gap between connections. Never replace it.
    with lock(endpoint["lock_file"]):
        _apply(config, endpoint)


def _apply(config, endpoint):
    policy = validate(config)
    database, owner, runtime, schema = [identifier(policy[key]) for key in
                                        ("database", "owner_role", "runtime_role", "schema")]
    membership = query(endpoint, "postgres", "SELECT EXISTS (SELECT 1 FROM pg_auth_members m "
                       "JOIN pg_roles r ON r.oid=m.member OR r.oid=m.roleid WHERE r.rolname IN "
                       f"('{policy['owner_role']}','{policy['runtime_role']}'));")
    if membership == "t":
        raise ValueError("dedicated application roles have unexpected role membership")
    unsafe = query(endpoint, "postgres", "SELECT EXISTS (SELECT 1 FROM pg_database WHERE "
                   f"datname='{policy['database']}' AND pg_get_userbyid(datdba)<>'{policy['owner_role']}') "
                   "OR EXISTS (SELECT 1 FROM pg_roles WHERE "
                   f"rolname IN ('{policy['owner_role']}','{policy['runtime_role']}') "
                   "AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)) "
                   "OR EXISTS (SELECT 1 FROM pg_shdepend d JOIN pg_roles r ON r.oid=d.refobjid "
                   "WHERE d.refclassid='pg_authid'::regclass AND d.deptype='o' "
                   f"AND r.rolname IN ('{policy['owner_role']}','{policy['runtime_role']}') "
                   f"AND d.dbid<>COALESCE((SELECT oid FROM pg_database WHERE datname='{policy['database']}'),0) "
                   "AND NOT (d.dbid=0 AND d.classid='pg_database'::regclass "
                   f"AND d.objid=(SELECT oid FROM pg_database WHERE datname='{policy['database']}') "
                   f"AND r.rolname='{policy['owner_role']}')); ")
    if unsafe == "t":
        raise ValueError("conflicting application role or database ownership; explicit adoption is required")
    sql = [f"SELECT pg_advisory_lock(hashtextextended('harbor-db:{policy['database']}',0));"]
    for role in (owner, runtime):
        name = role.strip('"')
        sql.extend([f"SELECT 'CREATE ROLE {role} LOGIN' WHERE NOT EXISTS "
                    f"(SELECT 1 FROM pg_roles WHERE rolname='{name}')\\gexec",
                    f"ALTER ROLE {role} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;"])
    sql.extend([f"SELECT 'CREATE DATABASE {database} OWNER {owner}' WHERE NOT EXISTS "
                f"(SELECT 1 FROM pg_database WHERE datname='{policy['database']}')\\gexec",
                ])
    query(endpoint, "postgres", "\n".join(sql))
    unsafe_objects = query(endpoint, policy["database"], "SELECT EXISTS (SELECT 1 FROM pg_class c "
                           "JOIN pg_namespace n ON n.oid=c.relnamespace "
                           f"WHERE n.nspname='{policy['schema']}' AND c.relkind IN ('r','p','v','m','f','S') "
                           f"AND pg_get_userbyid(c.relowner)<>'{policy['owner_role']}') "
                           "OR EXISTS (SELECT 1 FROM pg_namespace "
                           f"WHERE nspname='{policy['schema']}' AND pg_get_userbyid(nspowner)<>'{policy['owner_role']}' "
                           "AND NOT (nspname='public' AND pg_get_userbyid(nspowner)='pg_database_owner'));")
    if unsafe_objects == "t":
        raise ValueError("conflicting application object ownership; explicit adoption is required")
    sql = ["BEGIN;", f"SELECT pg_advisory_xact_lock(hashtextextended('harbor-db:{policy['database']}',0));",
           f"REVOKE ALL ON DATABASE {database} FROM PUBLIC, {runtime};",
           f"GRANT CONNECT ON DATABASE {database} TO {runtime};",
           f"CREATE SCHEMA IF NOT EXISTS {schema} AUTHORIZATION {owner};",
           f"ALTER SCHEMA {schema} OWNER TO {owner};",
           f"REVOKE ALL ON SCHEMA {schema} FROM PUBLIC, {runtime};",
           f"GRANT USAGE ON SCHEMA {schema} TO {runtime};",
           f"REVOKE ALL ON ALL TABLES IN SCHEMA {schema} FROM PUBLIC, {runtime};",
           f"REVOKE ALL ON ALL SEQUENCES IN SCHEMA {schema} FROM PUBLIC, {runtime};",
           # Table-level revocation does not remove older column-level grants.
           "SELECT format('REVOKE ALL (%I) ON TABLE %I.%I FROM PUBLIC, %I',a.attname,n.nspname,c.relname,"
           f"'{policy['runtime_role']}') FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid "
           f"JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{policy['schema']}' "
           "AND c.relkind IN ('r','p','v','m','f') AND a.attnum>0 AND NOT a.attisdropped\\gexec"]
    for kind, key in (("TABLES", "table_privileges"), ("SEQUENCES", "sequence_privileges")):
        for scope in ("", f" IN SCHEMA {schema}"):
            sql.append(f"ALTER DEFAULT PRIVILEGES FOR ROLE {owner}{scope} REVOKE ALL ON {kind} FROM PUBLIC, {runtime};")
        if policy[key]:
            granted = ",".join(policy[key])
            sql.extend([f"GRANT {granted} ON ALL {kind} IN SCHEMA {schema} TO {runtime};",
                        f"ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA {schema} GRANT {granted} ON {kind} TO {runtime};"])
    for table, values in policy["tables"].items():
        target = f"{schema}.{identifier(table)}"
        statements = f"REVOKE ALL ON TABLE {target} FROM {runtime};"
        if values:
            statements += f"GRANT {','.join(values)} ON TABLE {target} TO {runtime};"
        sql.append(f"SELECT '{statements}' WHERE to_regclass('{target}') IS NOT NULL\\gexec")
    sql.append("COMMIT;")
    query(endpoint, policy["database"], "\n".join(sql))
    if not check(policy, endpoint):
        raise ValueError("application provisioning did not converge to the declared privileges")


def check(config, endpoint):
    policy = validate(config)
    if query(endpoint, "postgres", role_check(policy)) != "t":
        return False
    runtime, owner, schema, database = [policy[key] for key in
                                      ("runtime_role", "owner_role", "schema", "database")]
    checks = [f"(SELECT pg_get_userbyid(datdba)='{owner}' FROM pg_database WHERE datname='{database}')",
              f"(SELECT pg_get_userbyid(nspowner)='{owner}' FROM pg_namespace WHERE nspname='{schema}')",
              f"has_schema_privilege('{runtime}','{schema}','USAGE')",
               f"NOT has_schema_privilege('{runtime}','{schema}','CREATE')",
               f"has_database_privilege('{runtime}','{database}','CONNECT')",
               f"NOT has_database_privilege('{runtime}','{database}','CREATE')",
               f"NOT has_database_privilege('{runtime}','{database}','TEMP')",
               "NOT EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles r ON r.oid=m.member OR r.oid=m.roleid "
              f"WHERE r.rolname IN ('{runtime}','{owner}'))"]
    checks.extend([
        "NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace "
        f"WHERE n.nspname='{schema}' AND c.relkind IN ('r','p','v','m','f','S') "
        f"AND pg_get_userbyid(c.relowner)<>'{owner}')",
        "NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace, "
        "LATERAL aclexplode(c.relacl) acl WHERE "
        f"n.nspname='{schema}' AND (acl.grantee=0 OR "
        f"(acl.grantee=(SELECT oid FROM pg_roles WHERE rolname='{runtime}') AND acl.is_grantable)))",
        "NOT EXISTS (SELECT 1 FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid "
        "JOIN pg_namespace n ON n.oid=c.relnamespace, LATERAL aclexplode(a.attacl) acl "
        f"WHERE n.nspname='{schema}' AND acl.grantee IN (0,(SELECT oid FROM pg_roles WHERE rolname='{runtime}')))"
    ])
    for privilege in sorted(TABLE):
        default = "true" if privilege in policy["table_privileges"] else "false"
        cases = " ".join(f"WHEN '{table}' THEN {'true' if privilege in values else 'false'}"
                         for table, values in policy["tables"].items())
        expected = f"CASE c.relname {cases} ELSE {default} END" if cases else default
        checks.append("NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace "
                      f"WHERE n.nspname='{schema}' AND c.relkind IN ('r','p','v','m','f') "
                      f"AND has_table_privilege('{runtime}',c.oid,'{privilege}') <> ({expected}))")
    for privilege in sorted(SEQUENCE):
        expected = "true" if privilege in policy["sequence_privileges"] else "false"
        checks.append("NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace "
                      f"WHERE CASE WHEN n.nspname='{schema}' AND c.relkind='S' THEN "
                      f"has_sequence_privilege('{runtime}',c.oid,'{privilege}') <> {expected} ELSE false END)")
    if query(endpoint, database, "BEGIN READ ONLY; SELECT COALESCE(" + " AND ".join(checks) + ",false); COMMIT;") != "t":
        return False
    defaults = json.loads(query(endpoint, database,
        "BEGIN READ ONLY; SELECT COALESCE(json_agg(json_build_array(d.defaclobjtype,"
        "d.defaclnamespace=0,acl.privilege_type,acl.is_grantable,acl.grantee=0)), '[]') "
        "FROM pg_default_acl d, LATERAL aclexplode(d.defaclacl) acl "
        f"WHERE d.defaclrole=(SELECT oid FROM pg_roles WHERE rolname='{owner}') "
        f"AND d.defaclnamespace IN (0,(SELECT oid FROM pg_namespace WHERE nspname='{schema}')) "
        "AND d.defaclobjtype IN ('r','S') "
        f"AND acl.grantee IN (0,(SELECT oid FROM pg_roles WHERE rolname='{runtime}')); COMMIT;"))
    expected = [[kind, False, privilege, False, False] for kind, key in
                (("r", "table_privileges"), ("S", "sequence_privileges")) for privilege in policy[key]]
    return sorted(defaults) == sorted(expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("command", choices=("apply", "check"))
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text())
        if set(config) != {"version", "policy", "endpoint"} or config["version"] != 1:
            raise ValueError("unsupported application provisioning manifest")
        if args.command == "apply":
            apply(config["policy"], config["endpoint"])
        elif not check(config["policy"], config["endpoint"]):
            return 2
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"harbor-db-provision: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
