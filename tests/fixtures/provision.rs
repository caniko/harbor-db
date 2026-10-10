//! Native application provisioning VM assertions; Python supplies transport only.
use clap::Parser;
use harbor_db::testing::protocol::{Client, Request};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LIFECYCLE_SECONDS: u64 = 3600;
const COMMAND_SECONDS: u64 = 900;
const CONFIG: &str = "/etc/harbor-db/demo-provision.json";

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_fd: i32,
    #[arg(long)]
    native_provision: String,
    #[arg(long)]
    python_provision: String,
    #[arg(long)]
    psql: String,
    #[arg(long)]
    runuser: String,
    #[arg(long)]
    systemctl: String,
    #[arg(long)]
    acceptance: PathBuf,
}

fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message.into()).into())
    }
}

struct Gate {
    client: Client,
    began: Instant,
    assertions: BTreeSet<String>,
}

impl Gate {
    fn record(&mut self, name: &str) -> Result<()> {
        require(
            !name.is_empty() && self.assertions.insert(name.into()),
            format!("empty or duplicate assertion: {name}"),
        )
    }

    fn call(&mut self, request: Request) -> Result<Value> {
        require(
            self.began.elapsed() < Duration::from_secs(LIFECYCLE_SECONDS),
            "provision lifecycle budget exhausted",
        )?;
        Ok(self.client.call(request)?)
    }

    fn execute(&mut self, argv: Vec<String>) -> Result<(i64, String)> {
        let value = self.call(Request::Execute {
            node: "machine".into(),
            argv,
            timeout_seconds: COMMAND_SECONDS,
        })?;
        Ok((
            value["exit_code"].as_i64().ok_or("missing exit_code")?,
            value["output"].as_str().ok_or("missing output")?.into(),
        ))
    }

    fn command(&mut self, argv: Vec<String>, expected: i64, name: &str) -> Result<String> {
        let (code, output) = self.execute(argv)?;
        require(
            code == expected,
            format!("{name}: expected exit {expected}, got {code}: {output}"),
        )?;
        self.record(name)?;
        Ok(output)
    }

    fn lifecycle(&mut self, request: Request, name: &str) -> Result<()> {
        let result = self.call(request)?;
        require(
            result["completed"] == true,
            format!("{name}: lifecycle incomplete"),
        )?;
        self.record(name)
    }

    fn unit(&mut self, args: &Args, unit: &str, name: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(COMMAND_SECONDS);
        loop {
            require(
                Instant::now() < deadline,
                format!("timed out waiting for {unit}"),
            )?;
            let (code, output) = self.execute(vec![
                args.systemctl.clone(),
                "is-active".into(),
                unit.into(),
            ])?;
            if code == 0 && output.trim() == "active" {
                return self.record(name);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn provision(
        &mut self,
        args: &Args,
        executable: &str,
        action: &str,
        code: i64,
        name: &str,
    ) -> Result<()> {
        self.command(
            vec![
                args.runuser.clone(),
                "-u".into(),
                "postgres".into(),
                "--".into(),
                executable.into(),
                "--config".into(),
                CONFIG.into(),
                action.into(),
            ],
            code,
            name,
        )?;
        Ok(())
    }

    fn sql_argv(args: &Args, user: &str, role: &str, sql: &str) -> Vec<String> {
        vec![
            args.runuser.clone(),
            "-u".into(),
            user.into(),
            "--".into(),
            args.psql.clone(),
            "-X".into(),
            "-w".into(),
            "-qAt".into(),
            "-h".into(),
            "/run/postgresql".into(),
            "-p".into(),
            "5432".into(),
            "-v".into(),
            "ON_ERROR_STOP=1".into(),
            "-d".into(),
            "demo".into(),
            "-U".into(),
            role.into(),
            "-c".into(),
            sql.into(),
        ]
    }

    fn projection(&mut self, args: &Args) -> Result<Value> {
        let (code, output) =
            self.execute(Self::sql_argv(args, "postgres", "postgres", PROJECTION))?;
        require(code == 0, format!("SQL state readback failed: {output}"))?;
        let value: Value = serde_json::from_str(output.trim())?;
        require(value.is_object(), "SQL readback must be an object")?;
        Ok(value)
    }

    fn equal(&mut self, args: &Args, expected: &Value, name: &str) -> Result<()> {
        let actual = self.projection(args)?;
        require(
            actual == *expected,
            format!("{name}: SQL projection mismatch\nexpected: {expected}\nactual: {actual}"),
        )?;
        self.record(name)
    }

    fn client_permissions(&mut self, args: &Args, phase: &str) -> Result<()> {
        self.command(Self::sql_argv(args, "demo", "demo_runtime",
            "BEGIN; SELECT * FROM documents ORDER BY id; SELECT * FROM history ORDER BY id; INSERT INTO documents VALUES (9); UPDATE documents SET id=10 WHERE id=9; DELETE FROM documents WHERE id=10; INSERT INTO history VALUES (9); ROLLBACK;"),
            0, &format!("{phase}:runtime-allowed-sql"))?;
        self.command(
            Self::sql_argv(args, "demo", "demo_runtime", "UPDATE history SET id=2"),
            1,
            &format!("{phase}:history-update-rejected"),
        )?;
        self.command(
            Self::sql_argv(args, "demo", "demo_owner", "SELECT 1"),
            2,
            &format!("{phase}:runtime-os-user-owner-login-rejected"),
        )?;
        Ok(())
    }

    fn services(&mut self, args: &Args, phase: &str) -> Result<()> {
        self.unit(
            args,
            "postgresql.service",
            &format!("{phase}:postgresql-active"),
        )?;
        self.unit(args, "demo.service", &format!("{phase}:demo-active"))?;
        for unit in [
            "harbor-db-demo-provision.service",
            "demo-schema.service",
            "harbor-db-demo-permissions.service",
        ] {
            let (code, output) = self.execute(vec![
                args.systemctl.clone(),
                "show".into(),
                unit.into(),
                "-p".into(),
                "Result".into(),
                "-p".into(),
                "ExecMainStatus".into(),
            ])?;
            let lines: BTreeSet<_> = output.lines().collect();
            require(
                code == 0 && lines.contains("Result=success") && lines.contains("ExecMainStatus=0"),
                format!("{unit} unhealthy: {output}"),
            )?;
            self.record(&format!("{phase}:{unit}-healthy"))?;
        }
        Ok(())
    }
}

// All identities use stable names. ACL entries are exploded and sorted rather
// than comparing catalog OIDs or ACL storage order. Include effective grants,
// column ACLs, default ACLs, row policies, role membership and acknowledged rows.
const PROJECTION: &str = r#"
BEGIN READ ONLY;
WITH acl_entries AS (
 SELECT 'database' AS kind, datname AS object, '' AS scope, a.* FROM pg_database d, LATERAL aclexplode(d.datacl) a WHERE datname='demo'
 UNION ALL SELECT 'schema', nspname, '', a.* FROM pg_namespace n, LATERAL aclexplode(n.nspacl) a WHERE nspname='public'
 UNION ALL SELECT 'relation', c.relname, n.nspname, a.* FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace, LATERAL aclexplode(c.relacl) a WHERE n.nspname='public'
 UNION ALL SELECT 'column', c.relname || '.' || at.attname, n.nspname, a.* FROM pg_attribute at JOIN pg_class c ON c.oid=at.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace, LATERAL aclexplode(at.attacl) a WHERE n.nspname='public' AND at.attnum>0 AND NOT at.attisdropped
 UNION ALL SELECT 'default-' || d.defaclobjtype::text, pg_get_userbyid(d.defaclrole), COALESCE(n.nspname,'<global>'), a.* FROM pg_default_acl d LEFT JOIN pg_namespace n ON n.oid=d.defaclnamespace, LATERAL aclexplode(d.defaclacl) a WHERE d.defaclrole IN (SELECT oid FROM pg_roles WHERE rolname IN ('demo_owner','demo_runtime'))
), acl_names AS (
 SELECT kind, object, scope, pg_get_userbyid(grantor) AS grantor,
 CASE WHEN grantee=0 THEN 'PUBLIC' ELSE pg_get_userbyid(grantee) END AS grantee,
 privilege_type, is_grantable FROM acl_entries
)
SELECT json_build_object(
 'roles', (SELECT json_agg(row_to_json(r) ORDER BY rolname) FROM (SELECT rolname,rolsuper,rolinherit,rolcreaterole,rolcreatedb,rolcanlogin,rolreplication,rolbypassrls,rolconnlimit,rolvaliduntil,rolconfig FROM pg_roles WHERE rolname IN ('demo_owner','demo_runtime')) r),
 'memberships', (SELECT COALESCE(json_agg(row_to_json(r) ORDER BY role,member,grantor),'[]') FROM (SELECT pg_get_userbyid(m.roleid) AS role,pg_get_userbyid(m.member) AS member,pg_get_userbyid(m.grantor) AS grantor,m.admin_option,m.inherit_option,m.set_option FROM pg_auth_members m WHERE m.roleid IN (SELECT oid FROM pg_roles WHERE rolname IN ('demo_owner','demo_runtime')) OR m.member IN (SELECT oid FROM pg_roles WHERE rolname IN ('demo_owner','demo_runtime'))) r),
 'database', (SELECT json_build_object('name',datname,'owner',pg_get_userbyid(datdba),'allow_connections',datallowconn,'connection_limit',datconnlimit) FROM pg_database WHERE datname='demo'),
 'schemas', (SELECT json_agg(json_build_array(nspname,pg_get_userbyid(nspowner)) ORDER BY nspname) FROM pg_namespace WHERE nspname='public'),
 'relations', (SELECT COALESCE(json_agg(json_build_array(n.nspname,c.relname,c.relkind,pg_get_userbyid(c.relowner),c.relrowsecurity,c.relforcerowsecurity) ORDER BY n.nspname,c.relname),'[]') FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public'),
 'acl', (SELECT COALESCE(json_agg(row_to_json(a) ORDER BY kind,object,scope,grantor,grantee,privilege_type,is_grantable),'[]') FROM acl_names a),
 'policies', (SELECT COALESCE(json_agg(row_to_json(r) ORDER BY schema,relation,name),'[]') FROM (SELECT n.nspname AS schema,c.relname AS relation,p.polname AS name,p.polcmd,p.polpermissive,(SELECT json_agg(CASE WHEN role=0 THEN 'PUBLIC' ELSE pg_get_userbyid(role) END ORDER BY CASE WHEN role=0 THEN 'PUBLIC' ELSE pg_get_userbyid(role) END) FROM unnest(p.polroles) role) AS roles,pg_get_expr(p.polqual,p.polrelid) AS qualifier,pg_get_expr(p.polwithcheck,p.polrelid) AS with_check FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public') r),
 'effective_table_grants', (SELECT json_agg(json_build_array(c.relname,p.privilege,has_table_privilege('demo_runtime',c.oid,p.privilege)) ORDER BY c.relname,p.privilege) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN unnest(ARRAY['SELECT','INSERT','UPDATE','DELETE','TRUNCATE','REFERENCES','TRIGGER','MAINTAIN']) p(privilege) WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m','f')),
 'effective_schema_database_grants', json_build_array(has_schema_privilege('demo_runtime','public','USAGE'),has_schema_privilege('demo_runtime','public','CREATE'),has_database_privilege('demo_runtime','demo','CONNECT'),has_database_privilege('demo_runtime','demo','CREATE'),has_database_privilege('demo_runtime','demo','TEMP')),
 'records', json_build_object('documents',(SELECT json_agg(id ORDER BY id) FROM documents),'history',(SELECT json_agg(id ORDER BY id) FROM history))
);
COMMIT;
"#;

fn run(args: Args) -> Result<()> {
    require(args.control_fd >= 0, "invalid control descriptor")?;
    for executable in [
        &args.native_provision,
        &args.python_provision,
        &args.psql,
        &args.runuser,
        &args.systemctl,
    ] {
        require(
            executable.starts_with("/nix/store/"),
            "fixture requires absolute packaged executables",
        )?;
    }
    require(
        args.native_provision != args.python_provision,
        "native and Python executables must differ",
    )?;
    // SAFETY: pass_fds transfers this inherited Unix socket to exactly one Client.
    let stream = unsafe { UnixStream::from_raw_fd(args.control_fd) };
    let mut gate = Gate {
        client: Client::new(stream, Duration::from_secs(COMMAND_SECONDS + 60))?,
        began: Instant::now(),
        assertions: BTreeSet::new(),
    };
    gate.lifecycle(
        Request::Start {
            node: "machine".into(),
            allow_reboot: true,
        },
        "machine-started-with-reboot",
    )?;
    gate.services(&args, "boot")?;
    let (code, output) = gate.execute(vec![
        args.systemctl.clone(),
        "show".into(),
        "harbor-db-demo-permissions.service".into(),
        "-p".into(),
        "ExecStart".into(),
        "--value".into(),
    ])?;
    require(
        code == 0 && output.contains(&args.native_provision),
        format!("boot did not use native provisioner: {output}"),
    )?;
    gate.record("boot-policy-established-by-native-service")?;
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        0,
        "boot:native-check",
    )?;
    gate.provision(
        &args,
        &args.python_provision,
        "check",
        0,
        "boot:python-accepts-native-policy",
    )?;
    gate.client_permissions(&args, "boot")?;
    let mut baseline = gate.projection(&args)?;
    require(
        baseline["records"] == json!({"documents":[1],"history":[1]}),
        format!("initial acknowledged records: {baseline}"),
    )?;
    gate.record("boot:acknowledged-records-read")?;
    gate.command(
        vec![
            args.systemctl.clone(),
            "start".into(),
            "incomplete.service".into(),
        ],
        1,
        "incomplete-schema-runtime-start-rejected",
    )?;
    let (code, output) = gate.execute(vec![
        args.systemctl.clone(),
        "show".into(),
        "incomplete-schema.service".into(),
        "-p".into(),
        "Result".into(),
        "--value".into(),
    ])?;
    require(
        code == 0 && output.trim() == "success",
        format!("incomplete schema unit must finish successfully: {output}"),
    )?;
    gate.record("incomplete-schema-success-cannot-admit-missing-declared-table")?;
    gate.command(
        vec![
            args.systemctl.clone(),
            "is-active".into(),
            "incomplete.service".into(),
        ],
        3,
        "incomplete-runtime-remains-inactive",
    )?;
    for (implementation, executable) in [
        ("native", &args.native_provision),
        ("python", &args.python_provision),
    ] {
        for (action, code) in [("check", 2), ("reconcile", 1)] {
            gate.command(
                vec![
                    args.runuser.clone(),
                    "-u".into(),
                    "postgres".into(),
                    "--".into(),
                    executable.clone(),
                    "--config".into(),
                    "/etc/harbor-db/incomplete-provision.json".into(),
                    action.into(),
                ],
                code,
                &format!("incomplete-schema:{implementation}-{action}-rejects-missing-table"),
            )?;
        }
    }
    gate.provision(
        &args,
        &args.python_provision,
        "apply",
        0,
        "python-applies-native-policy",
    )?;
    gate.equal(
        &args,
        &baseline,
        "python-apply-preserves-complete-native-sql-state",
    )?;
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        0,
        "native-accepts-python-policy",
    )?;
    gate.command(
        Gate::sql_argv(
            &args,
            "postgres",
            "postgres",
            "GRANT UPDATE ON history TO demo_runtime",
        ),
        0,
        "overgrant-injected",
    )?;
    let overgrant = gate.projection(&args)?;
    require(
        overgrant != baseline && overgrant["records"] == baseline["records"],
        "overgrant not observed or changed records",
    )?;
    gate.record("overgrant-observed-in-sql-readback")?;
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        2,
        "native-check-exit-two-for-overgrant",
    )?;
    gate.provision(
        &args,
        &args.python_provision,
        "check",
        2,
        "python-check-exit-two-for-overgrant",
    )?;
    gate.provision(
        &args,
        &args.python_provision,
        "apply",
        0,
        "python-repairs-overgrant",
    )?;
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        0,
        "native-accepts-python-repair",
    )?;
    gate.equal(
        &args,
        &baseline,
        "python-repair-converges-complete-sql-state",
    )?;
    gate.command(
        Gate::sql_argv(
            &args,
            "postgres",
            "postgres",
            "GRANT UPDATE ON history TO demo_runtime",
        ),
        0,
        "service-repair-overgrant-injected",
    )?;
    gate.command(
        vec![
            args.systemctl.clone(),
            "restart".into(),
            "harbor-db-demo-permissions.service".into(),
        ],
        0,
        "native-permissions-service-restarted",
    )?;
    gate.services(&args, "service-repair")?;
    // Permissions restart also restarts the enrolled client unit. Its declared
    // INSERTs acknowledge one additional row per table; privilege repair must
    // preserve the entire original projection alongside those exact new rows.
    baseline["records"] = json!({"documents":[1,1],"history":[1,1]});
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        0,
        "native-service-repair-check",
    )?;
    gate.provision(
        &args,
        &args.python_provision,
        "check",
        0,
        "python-accepts-native-service-repair",
    )?;
    gate.equal(
        &args,
        &baseline,
        "native-service-repair-converges-complete-sql-state",
    )?;
    for round in 1..=2 {
        for (implementation, executable) in [
            ("native", &args.native_provision),
            ("python", &args.python_provision),
        ] {
            gate.provision(
                &args,
                executable,
                "apply",
                0,
                &format!("repeat-{round}:{implementation}-apply"),
            )?;
            gate.provision(
                &args,
                executable,
                "reconcile",
                0,
                &format!("repeat-{round}:{implementation}-reconcile"),
            )?;
            gate.equal(
                &args,
                &baseline,
                &format!("repeat-{round}:{implementation}-complete-state-and-records-unchanged"),
            )?;
        }
    }
    gate.lifecycle(
        Request::Reboot {
            node: "machine".into(),
        },
        "machine-rebooted",
    )?;
    gate.services(&args, "reboot")?;
    gate.provision(
        &args,
        &args.native_provision,
        "check",
        0,
        "reboot:native-check",
    )?;
    gate.provision(
        &args,
        &args.python_provision,
        "check",
        0,
        "reboot:python-check",
    )?;
    gate.client_permissions(&args, "reboot")?;
    let mut after_reboot = baseline.clone();
    after_reboot["records"] = json!({"documents":[1,1,1],"history":[1,1,1]});
    gate.equal(
        &args,
        &after_reboot,
        "reboot:grants-intact-and-real-client-records-acknowledged",
    )?;
    require(!gate.assertions.is_empty(), "no semantic assertions")?;
    let assertions: Vec<_> = gate
        .assertions
        .into_iter()
        .map(|name| json!({"name":name,"passed":true}))
        .collect();
    harbor_db::storage::durable::write_json(
        &args.acceptance,
        &json!({
            "schema":1,
            "case_id":harbor_db::testing::runner::safe_identity("vm.x86_64-linux.native-application-provision"),
            "assertions":assertions,
        }),
    )?;
    Ok(())
}

fn main() -> Result<()> {
    let (finished, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if receiver
            .recv_timeout(Duration::from_secs(LIFECYCLE_SECONDS))
            .is_err()
        {
            eprintln!("provision overall 3600-second lifecycle deadline exceeded");
            std::process::exit(1);
        }
    });
    let result = run(Args::parse());
    let _ = finished.send(());
    result
}
