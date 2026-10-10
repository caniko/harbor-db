//! Persistent authority and lifetime consumer leases.
use super::{Result, durable, invalid, process, string};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn directory(p: &Path) -> Result<()> {
    if !p.is_absolute() || !p.is_dir() || std::fs::canonicalize(p)? != p {
        return Err(invalid(format!(
            "storage directory is missing or redirected: {}",
            p.display()
        )));
    }
    Ok(())
}
pub fn state_directory(c: &Value) -> Result<PathBuf> {
    super::pg_core::require_mounts(c)?;
    let p = PathBuf::from(string(c, "state_dir")?);
    directory(&p)?;
    Ok(p)
}
fn strings(v: Option<&Value>) -> Result<Vec<String>> {
    match v {
        None => Ok(vec![]),
        Some(v) => v
            .as_array()
            .ok_or_else(|| invalid("invalid storage paths"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("invalid storage path"))
            })
            .collect(),
    }
}
pub fn require_files(files: &[String]) -> Result<()> {
    for s in files {
        let p = Path::new(s);
        if !p.is_absolute()
            || !p.is_file()
            || std::fs::canonicalize(p)? != p
            || std::fs::metadata(p)?.len() == 0
        {
            return Err(invalid(format!(
                "required storage file is missing, empty or redirected: {s}"
            )));
        }
    }
    Ok(())
}
fn counter(v: &Value) -> Result<String> {
    let s = v
        .as_number()
        .ok_or_else(|| invalid("invalid consumer storage counters"))?
        .to_string();
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("invalid consumer storage counters"));
    }
    Ok(s)
}
pub fn contract(c: &Value) -> Result<Value> {
    let state = state_directory(c)?;
    let name = string(c, "resource")?;
    if !valid_name(name) {
        return Err(invalid("invalid resource name"));
    }
    let mut consumer = json!({});
    if let Some(cmd) = c.get("consumer_command") {
        let argv = strings(Some(cmd))?;
        if !argv.is_empty() {
            let mut command = std::process::Command::new(&argv[0]);
            command.args(&argv[1..]);
            let output = process::output(&mut command)?;
            if !output.status.success() {
                return Err(invalid(format!(
                    "consumer storage validation failed: {}",
                    process::text(&output.stderr)?.trim()
                )));
            }
            consumer = serde_json::from_str(&process::text(&output.stdout)?)?;
            let m = consumer
                .as_object()
                .ok_or_else(|| invalid("invalid consumer storage contract"))?;
            if m.keys().any(|k| {
                ![
                    "binding",
                    "directories",
                    "required_files",
                    "minimum_counters",
                ]
                .contains(&k.as_str())
            }) || ["binding", "directories", "required_files"]
                .iter()
                .any(|k| !m.contains_key(*k))
                || !consumer["binding"].is_object()
            {
                return Err(invalid("invalid consumer storage contract"));
            }
        }
    }
    let dirs: BTreeSet<_> = strings(c.get("directories"))?
        .into_iter()
        .chain(strings(consumer.get("directories"))?)
        .collect();
    if dirs.is_empty() {
        return Err(invalid("authority requires at least one storage directory"));
    }
    for d in &dirs {
        directory(Path::new(d))?;
        if state.starts_with(d) {
            return Err(invalid(
                "authority state must be outside the guarded directories",
            ));
        }
    }
    let files: BTreeSet<_> = strings(c.get("required_files"))?
        .into_iter()
        .chain(strings(consumer.get("required_files"))?)
        .collect();
    require_files(&files.iter().cloned().collect::<Vec<_>>())?;
    let mut binding = c
        .get("binding")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("invalid storage binding"))?
        .clone();
    if consumer.get("binding").is_some() {
        binding.insert("consumer".into(), consumer["binding"].clone());
    }
    let mut result = json!({"version":1,"resource":name,"binding":binding,"directories":dirs,"required_files":files});
    if let Some(v) = consumer.get("minimum_counters") {
        let m = v
            .as_object()
            .ok_or_else(|| invalid("invalid consumer storage counters"))?;
        for v in m.values() {
            counter(v)?;
        }
        if !m.is_empty() {
            result["minimum_counters"] = v.clone();
        }
    }
    Ok(result)
}
pub fn anchor(c: &Value, d: &Path) -> Result<PathBuf> {
    Ok(d.join(format!(
        ".harbor-db-{}-identity.json",
        string(c, "resource")?
    )))
}
pub fn verify(c: &Value, e: &Value) -> Result<Value> {
    let r = durable::read_json(&Path::new(string(c, "state_dir")?).join("identity.json"))?;
    for k in ["version", "resource", "binding", "directories"] {
        if r.get(k).is_none() || r.get(k) != e.get(k) {
            return Err(invalid(
                "storage authority mismatch: backend, schema or paths changed",
            ));
        }
    }
    require_files(&strings(r.get("required_files"))?)?;
    if let Some(v) = r.get("minimum_counters") {
        for (k, v) in v
            .as_object()
            .ok_or_else(|| invalid("invalid adopted counters"))?
        {
            let a = counter(v)?;
            let b = counter(
                e.get("minimum_counters")
                    .and_then(|v| v.get(k))
                    .ok_or_else(|| {
                        invalid("consumer storage is older or incomplete compared with adoption")
                    })?,
            )?;
            if (b.len(), &b) < (a.len(), &a) {
                return Err(invalid(
                    "consumer storage is older or incomplete compared with adoption",
                ));
            }
        }
    }
    let marker = json!({"resource":c["resource"],"identity":r.get("identity").ok_or_else(||invalid("missing storage identity"))?});
    for d in strings(e.get("directories"))? {
        if durable::read_json(&anchor(c, Path::new(&d))?)? != marker {
            return Err(invalid(format!("storage identity mismatch at {d}")));
        }
    }
    Ok(r)
}
pub fn require_stable(c: &Value) -> Result<()> {
    let p = Path::new(string(c, "state_dir")?).join("transition.json");
    if p.exists()
        && !["planned", "write-enabled", "complete", "aborted"]
            .contains(&string(&durable::read_json(&p)?, "phase")?)
    {
        return Err(invalid(
            "application backend transition is unfinished; ordinary startup is inhibited",
        ));
    }
    Ok(())
}
pub fn inspection(c: &Value) -> Result<(durable::Lease, Value)> {
    let s = state_directory(c)?;
    if !s.join("identity.json").exists() {
        return Err(invalid(
            "storage is not adopted; explicit adoption is required",
        ));
    }
    let l = durable::lock(&s.join("lock"), true, false)?;
    require_stable(c)?;
    let r = verify(c, &contract(c)?)?;
    Ok((l, r))
}
pub fn check(c: &Value) -> Result<()> {
    let _guard = inspection(c)?;
    Ok(())
}
pub fn adoption(c: &Value, id: &str) -> Result<durable::Lease> {
    if id.is_empty() || id.chars().count() > 128 {
        return Err(invalid(
            "a verified nonempty storage identifier is required",
        ));
    }
    let s = state_directory(c)?;
    durable::lock(&s.join("lock"), false, !s.join("identity.json").exists())
}
/// Publish only while retaining the exclusive lease returned by `adoption`.
pub fn publish_adoption(c: &Value, id: &str) -> Result<Value> {
    let s = state_directory(c)?;
    let mut e = contract(c)?;
    if s.join("identity.json").exists() {
        let r = verify(c, &e)?;
        if r["identity"] != id {
            return Err(invalid("cannot replace adopted storage identity"));
        }
        return Ok(r);
    }
    let marker = json!({"resource":c["resource"],"identity":id});
    let dirs = strings(e.get("directories"))?;
    for d in &dirs {
        let p = anchor(c, Path::new(d))?;
        if p.exists() && durable::read_json(&p)? != marker {
            return Err(invalid(format!("existing storage identity differs at {d}")));
        }
    }
    for d in dirs {
        durable::write_json(&anchor(c, Path::new(&d))?, &marker)?;
    }
    e["identity"] = json!(id);
    durable::write_json(&s.join("identity.json"), &e)?;
    Ok(e)
}
pub fn adopt(c: &Value, id: &str) -> Result<Value> {
    let _lease = adoption(c, id)?;
    publish_adoption(c, id)
}
pub fn serve(c: &Value, argv: &[String]) -> Result<()> {
    let exe = argv
        .first()
        .ok_or_else(|| invalid("consumer executable must be an absolute path"))?;
    if !Path::new(exe).is_absolute() {
        return Err(invalid("consumer executable must be an absolute path"));
    }
    let (lease, _) = inspection(c)?;
    let mut command = std::process::Command::new(exe);
    command.args(&argv[1..]);
    super::process::exec(&mut command, &[lease.fd()])
}
