//! Individual test adapters turn real harness results into semantic receipts.
use super::supervisor::{self, Result};
use crate::storage::{codec, durable};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorSpec {
    pub schema: u32,
    pub case_id: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workspace: PathBuf,
    /// A known harness selector is required for Rust/Python test execution.
    pub selector: Option<String>,
    #[serde(default)]
    pub prerequisite: Option<(PathBuf, u32)>,
}

pub fn execute(spec: &ExecutorSpec) -> Result<()> {
    if spec.schema != 1 || spec.argv.is_empty() || !spec.workspace.is_absolute() {
        return Err(supervisor::error("invalid executor specification"));
    }
    let stdout = spec.workspace.join("stdout.log");
    let stderr = spec.workspace.join("stderr.log");
    let log = |path: &std::path::Path| -> std::io::Result<std::fs::File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    };
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(log(&stdout)?)
        .stderr(log(&stderr)?);
    let status = crate::storage::process::spawn(&mut command)?.wait()?;
    let bounded = |path: &std::path::Path| -> Result<String> {
        let mut bytes = Vec::new();
        durable::open_regular(path, false)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(supervisor::error(
                "test harness output exceeds validation bound",
            ));
        }
        Ok(String::from_utf8(bytes)?)
    };
    let out = bounded(&stdout)?;
    let err = bounded(&stderr)?;
    std::io::stdout().write_all(out.as_bytes())?;
    std::io::stderr().write_all(err.as_bytes())?;
    if !status.success() {
        return Err(supervisor::error(format!("test harness failed: {status}")));
    }
    let assertion = match spec.selector.as_deref() {
        Some(selector) if spec.argv[0].ends_with("cargo") || spec.argv[0] == "cargo" => {
            let name = format!("test {selector} ... ok");
            if !out.lines().any(|line| line == name)
                || !out.contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
            {
                return Err(supervisor::error(
                    "Rust exact selector did not execute exactly one passing test",
                ));
            }
            format!("Rust harness executed {selector}")
        }
        Some(selector) if spec.argv.iter().any(|a| a == "unittest") => {
            if !err.lines().any(|line| line.starts_with("Ran 1 test in "))
                || !err.lines().any(|line| line == "OK")
                || err.contains("skipped=")
                || !err.contains(selector.rsplit('.').next().unwrap_or(selector))
            {
                return Err(supervisor::error(
                    "Python exact selector did not execute exactly one passing test",
                ));
            }
            format!("Python unittest executed {selector}")
        }
        Some(_) => {
            return Err(supervisor::error(
                "unregistered exact-selector test harness",
            ));
        }
        None => {
            let (package, major) = spec
                .prerequisite
                .as_ref()
                .ok_or_else(|| supervisor::error("command has no semantic assertion adapter"))?;
            let required = [
                "initdb",
                "pg_ctl",
                "psql",
                "pg_dump",
                "pg_restore",
                "postgres",
                "pg_upgrade",
            ];
            for name in required {
                let path = package.join("bin").join(name);
                if !path.is_file() || path.metadata()?.permissions().mode() & 0o111 == 0 {
                    return Err(supervisor::error(format!(
                        "missing executable disposable PostgreSQL tool: {name}"
                    )));
                }
            }
            let mut command = crate::storage::process::CommandSpec::new(vec![
                package.join("bin/postgres").to_string_lossy().into_owned(),
                "--version".into(),
            ]);
            command.timeout = std::time::Duration::from_secs(10);
            let version = String::from_utf8(crate::storage::process::execute(&command)?)?;
            if !version.starts_with(&format!("postgres (PostgreSQL) {major}.")) {
                return Err(supervisor::error(
                    "disposable PostgreSQL major differs from catalog",
                ));
            }
            format!("Disposable PostgreSQL {major} executable inventory and version verified")
        }
    };
    durable::write_json(
        &spec.workspace.join("acceptance.json"),
        &serde_json::json!({
            "schema":1,"case_id":spec.case_id,"assertions":[{"name":assertion,"passed":true}]
        }),
    )?;
    durable::write_json(
        &spec.workspace.join("log-bindings.json"),
        &serde_json::json!({
            "version":1,"stdout_sha256":codec::file_digest(&stdout)?,"stderr_sha256":codec::file_digest(&stderr)?
        }),
    )?;
    Ok(())
}

pub fn load(path: &std::path::Path) -> Result<ExecutorSpec> {
    Ok(serde_json::from_slice(&super::evidence::bounded_read(
        path,
    )?)?)
}
