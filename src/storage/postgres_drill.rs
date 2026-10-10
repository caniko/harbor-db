//! Logical restoration using a private, disposable local PostgreSQL endpoint.
use super::{Result, invalid, pg_core, process};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    time::{Duration, Instant},
};

fn run(package: &Path, program: &str, args: Vec<String>, deadline: Instant) -> Result<()> {
    let mut argv = vec![package.join("bin").join(program).display().to_string()];
    argv.extend(args);
    let mut spec = pg_core::command(argv, true);
    spec.timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| invalid("disposable restore exceeded its execution limit"))?;
    process::execute(&spec)
        .map_err(|_| invalid(format!("disposable PostgreSQL {program} failed")))?;
    Ok(())
}

pub fn operate(
    package: &Path,
    command: &str,
    backup: &Path,
    workspace: &Path,
    dump: &str,
) -> Result<()> {
    operate_with_timeout(
        package,
        command,
        backup,
        workspace,
        dump,
        timeout_seconds(None)?,
    )
}

/// The owning application adapter supplies its contract budget. Standalone
/// callers use the same 1800-second default or an explicit bounded CLI value.
pub fn timeout_seconds(explicit: Option<u64>) -> Result<u64> {
    let seconds = match explicit {
        Some(seconds) => seconds,
        None => match std::env::var("HARBOR_DB_APPLICATION_TIMEOUT_SECONDS") {
            Ok(value) => value
                .parse()
                .map_err(|_| invalid("invalid disposable restore timeout"))?,
            Err(std::env::VarError::NotPresent) => 1800,
            Err(_) => return Err(invalid("invalid disposable restore timeout")),
        },
    };
    if !(1..=86400).contains(&seconds) {
        return Err(invalid("invalid disposable restore timeout"));
    }
    Ok(seconds)
}

pub fn operate_with_timeout(
    package: &Path,
    command: &str,
    backup: &Path,
    workspace: &Path,
    dump: &str,
    timeout: u64,
) -> Result<()> {
    let timeout = timeout_seconds(Some(timeout))?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    if !pg_core::unredirected(workspace)? || !pg_core::unredirected(backup)? {
        return Err(invalid(
            "disposable restore paths must be absolute and unredirected",
        ));
    }
    let info = fs::metadata(workspace)?;
    // SAFETY: getuid has no preconditions or borrowed pointers.
    if info.uid() != unsafe { libc::getuid() } || info.mode() & 0o077 != 0 {
        return Err(invalid("disposable workspace must be owned and private"));
    }
    if Path::new(dump).file_name().and_then(|s| s.to_str()) != Some(dump) {
        return Err(invalid("dump must be a direct backup child"));
    }
    let cluster = workspace.join("cluster");
    let socket = workspace.join("socket");
    let data = cluster.display().to_string();
    if command == "cleanup" {
        if cluster.join("postmaster.pid").exists() {
            run(
                package,
                "pg_ctl",
                vec![
                    "-D".into(),
                    data,
                    "-m".into(),
                    "fast".into(),
                    "-w".into(),
                    "stop".into(),
                ],
                deadline,
            )?;
        }
        return Ok(());
    }
    if command != "restore" {
        return Err(invalid("unsupported disposable restore command"));
    }
    if cluster.exists() || socket.exists() {
        return Err(invalid("disposable restore requires a new workspace"));
    }
    fs::DirBuilder::new().mode(0o700).create(&socket)?;
    let result = (|| {
        run(
            package,
            "initdb",
            vec![
                "-D".into(),
                data.clone(),
                "--locale=C".into(),
                "--encoding=UTF8".into(),
                "--auth=trust".into(),
            ],
            deadline,
        )?;
        run(
            package,
            "pg_ctl",
            vec![
                "-D".into(),
                data,
                "-l".into(),
                "/dev/null".into(),
                "-o".into(),
                format!(
                    "-k {} -p 55439 -c listen_addresses=",
                    shell_quote(&socket.display().to_string())
                ),
                "-w".into(),
                "start".into(),
            ],
            deadline,
        )?;
        let mut endpoint = vec![
            "-h".into(),
            socket.display().to_string(),
            "-p".into(),
            "55439".into(),
        ];
        let mut created = endpoint.clone();
        created.push("harbor_restore".into());
        run(package, "createdb", created, deadline)?;
        let mut restore = vec![
            "--exit-on-error".into(),
            "--no-owner".into(),
            "--no-acl".into(),
        ];
        restore.append(&mut endpoint);
        restore.extend([
            "-d".into(),
            "harbor_restore".into(),
            backup.join(dump).display().to_string(),
        ]);
        run(package, "pg_restore", restore, deadline)
    })();
    if result.is_err() {
        // Cleanup has its own bounded attempt after the restore budget expires.
        operate_with_timeout(
            package,
            "cleanup",
            backup,
            workspace,
            dump,
            timeout.min(120),
        )?;
    }
    result
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
