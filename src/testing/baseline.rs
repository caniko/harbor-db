//! Frozen PR provenance and admission gates for the runtime migration.
//! Static admission proves retained scope and mappings, never runtime parity.
use super::{
    catalog::{Execution, Suite},
    supervisor::{Result, error},
};
use crate::storage::codec;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

pub const PR14_HEAD: &str = "4b2850507a2e9bdfe198caf9178ea7b99ffb03a5";
pub const PR14_BASE: &str = "38ebf2cfbca678cc1ab79a96d6968002fb382b5e";
const PR14_FILES: &str = "862c3b34110f00d3661e12c4fedda4c455de0f7382f4a39d2cb63c98ac404fc4";
const PR14_GATES: &str = "71736e6d0aadfcde534f3d755ab836543589b42cbb77289df66d43f8d6b6ae83";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub version: u32,
    pub pull_request: String,
    pub base: String,
    pub head: String,
    pub qualification: String,
    pub python_test_count: usize,
    pub python_inventory_sha256: String,
    pub gates: Vec<String>,
    pub files: Vec<SourceFile>,
    pub modules: Vec<ModuleMapping>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: PathBuf,
    pub sha256: String,
    /// Legacy source/tests stay exact; Nix/docs/contracts deliberately migrate.
    pub role: FileRole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileRole {
    Runtime,
    Test,
    Contract,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleMapping {
    pub source: PathBuf,
    pub rust: Vec<PathBuf>,
    pub commands: Vec<String>,
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Baseline {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let baseline: Self = toml::from_str(&fs::read_to_string(path)?)?;
        baseline.validate()?;
        Ok(baseline)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.pull_request != "https://github.com/caniko/harbor-db/pull/14"
            || self.base != PR14_BASE
            || self.head != PR14_HEAD
            || self.qualification != "https://github.com/caniko/harbor-db/actions/runs/37910539266"
            || self.python_test_count != 174
            || self.gates.len() != 22
            || !digest(&self.python_inventory_sha256)
        {
            return Err(error(
                "unreviewed PR14 baseline or qualification provenance",
            ));
        }
        let mut files = BTreeSet::new();
        for file in &self.files {
            if !relative(&file.path) || !digest(&file.sha256) || !files.insert(&file.path) {
                return Err(error("invalid or duplicate baseline file"));
            }
        }
        let mut inventory: Vec<_> = self
            .files
            .iter()
            .map(|file| {
                let role = match file.role {
                    FileRole::Runtime => "runtime",
                    FileRole::Test => "test",
                    FileRole::Contract => "contract",
                };
                format!("{}\t{role}\t{}", file.path.display(), file.sha256)
            })
            .collect();
        inventory.sort();
        let mut original_gates = self.gates.clone();
        original_gates.sort();
        if codec::digest((inventory.join("\n") + "\n").as_bytes()) != PR14_FILES
            || codec::digest((original_gates.join("\n") + "\n").as_bytes()) != PR14_GATES
        {
            return Err(error("PR14 source scope or qualified gate set changed"));
        }
        let runtime: BTreeSet<_> = self
            .files
            .iter()
            .filter(|f| f.role == FileRole::Runtime)
            .map(|f| &f.path)
            .collect();
        let mut mapped = BTreeSet::new();
        let mut commands = BTreeSet::new();
        for module in &self.modules {
            if !mapped.insert(&module.source)
                || !runtime.contains(&module.source)
                || module.rust.is_empty()
                || module.rust.iter().any(|p| !relative(p))
            {
                return Err(error("missing, invalid or duplicate runtime mapping"));
            }
            for command in &module.commands {
                if !command.starts_with("harbor-db-") || !commands.insert(command) {
                    return Err(error("invalid or duplicate public runtime command"));
                }
            }
        }
        if runtime != mapped || runtime.len() != 16 {
            return Err(error("PR14 runtime mapping is incomplete"));
        }
        let gates: BTreeSet<_> = self.gates.iter().collect();
        if gates.len() != self.gates.len()
            || self.gates.iter().any(|g| {
                !g.starts_with(".#checks.x86_64-linux.")
                    && !g.starts_with(".#packages.x86_64-linux.")
            })
        {
            return Err(error("invalid or duplicate PR14 gate"));
        }
        Ok(())
    }

    /// Frozen source makes this narrow extractor deterministic. New prototypes
    /// may add IDs; each original method must remain registered independently.
    pub fn python_tests(&self, root: &Path) -> Result<BTreeSet<String>> {
        let mut methods = BTreeSet::new();
        for file in self.files.iter().filter(|f| f.role == FileRole::Test) {
            let bytes = fs::read(root.join(&file.path))?;
            if codec::digest(&bytes) != file.sha256 {
                return Err(error(format!(
                    "PR14 Python test baseline changed: {}",
                    file.path.display()
                )));
            }
            let text = std::str::from_utf8(&bytes)?;
            let module = file
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| error("invalid test module"))?;
            let mut class = None;
            for line in text.lines() {
                if let Some(name) = line.strip_prefix("class ") {
                    class = Some(
                        name.split(['(', ':'])
                            .next()
                            .ok_or_else(|| error("invalid test class"))?,
                    );
                } else if let Some(name) = line.strip_prefix("    def test_") {
                    let name = name
                        .split('(')
                        .next()
                        .ok_or_else(|| error("invalid test method"))?;
                    let class = class.ok_or_else(|| error("test method has no class"))?;
                    methods.insert(format!("{module}.{class}.test_{name}"));
                }
            }
        }
        let serialized = methods
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        if methods.len() != self.python_test_count
            || codec::digest(serialized.as_bytes()) != self.python_inventory_sha256
        {
            return Err(error(
                "PR14 Python method inventory differs from frozen baseline",
            ));
        }
        Ok(methods)
    }

    pub fn validate_migration(&self, root: &Path, suite: &Suite) -> Result<()> {
        self.validate()?;
        suite.validate()?;
        for file in self.files.iter().filter(|f| f.role == FileRole::Runtime) {
            if codec::digest(&fs::read(root.join(&file.path))?) != file.sha256 {
                return Err(error(format!(
                    "PR14 Python runtime baseline changed: {}",
                    file.path.display()
                )));
            }
        }
        for mapping in &self.modules {
            for path in &mapping.rust {
                if !root.join(path).is_file() {
                    return Err(error(format!(
                        "Rust counterpart missing: {}",
                        path.display()
                    )));
                }
            }
            for command in &mapping.commands {
                if !root.join(format!("src/bin/{command}.rs")).is_file() {
                    return Err(error(format!("PR14 public command missing: {command}")));
                }
                if !suite
                    .capabilities
                    .iter()
                    .flat_map(|c| &c.operations)
                    .any(|op| op.starts_with(&format!("{command}.")))
                {
                    return Err(error(format!(
                        "PR14 public command unregistered: {command}"
                    )));
                }
            }
        }
        let registered: BTreeSet<_> = suite
            .cases
            .iter()
            .filter_map(|c| c.python_migration_id.clone())
            .collect();
        let required = self.python_tests(root)?;
        if !required.is_subset(&registered) {
            return Err(error("PR14 Python migration case missing"));
        }
        let gates: BTreeSet<_> = suite
            .cases
            .iter()
            .filter_map(|c| match &c.execution {
                Execution::Nix { installable } => Some(installable.as_str()),
                _ => None,
            })
            .collect();
        for gate in &self.gates {
            if !gates.contains(gate.as_str()) {
                return Err(error(format!(
                    "PR14 qualification gate unregistered: {gate}"
                )));
            }
        }
        let ci: toml::Value = toml::from_str(&fs::read_to_string(root.join("simit.toml"))?)?;
        let ci = ci
            .get("ci")
            .and_then(|c| c.get("nix_builds"))
            .and_then(toml::Value::as_array)
            .ok_or_else(|| error("Simit Nix gate selection missing"))?;
        for gate in &self.gates {
            if !ci.iter().any(|value| value.as_str() == Some(gate)) {
                return Err(error(format!(
                    "PR14 qualification gate removed from CI: {gate}"
                )));
            }
        }
        Ok(())
    }
}
