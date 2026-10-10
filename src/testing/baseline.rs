//! Frozen PR provenance and admission gates for the runtime migration.
//! Static admission proves retained scope and mappings, never runtime parity.
use super::{
    catalog::{Execution, Profile, Suite},
    evidence::ArtifactKind,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeExtensions {
    version: u32,
    baseline_head: String,
    gates: Vec<String>,
    qualified_cases: Vec<String>,
    extensions: Vec<RuntimeExtension>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeExtension {
    path: PathBuf,
    sha256: String,
}

impl RuntimeExtensions {
    fn validate(&self, baseline: &Baseline, root: &Path, suite: &Suite) -> Result<()> {
        const GATES: [&str; 3] = [
            ".#checks.x86_64-linux.postgres-lifecycle-test",
            ".#checks.x86_64-linux.postgres-lifecycle-oracle-test",
            ".#checks.x86_64-linux.native-source-local-recovery",
        ];
        const CASES: [&str; 2] = [
            "nix.x86_64-linux.postgres-lifecycle-oracle-test",
            "vm.x86_64-linux.native-source-local-recovery",
        ];
        if self.version != 1 || self.baseline_head != PR14_HEAD || self.extensions.is_empty() {
            return Err(error(
                "invalid runtime extension version, head or empty scope",
            ));
        }
        for (values, required) in [
            (&self.gates, GATES.as_slice()),
            (&self.qualified_cases, CASES.as_slice()),
        ] {
            let unique: BTreeSet<_> = values.iter().map(String::as_str).collect();
            if unique.len() != values.len()
                || unique.iter().any(|v| v.trim().is_empty())
                || !required.iter().all(|v| unique.contains(v))
            {
                return Err(error(
                    "runtime extension required gate or case missing or duplicate",
                ));
            }
        }
        let mut declared = BTreeSet::new();
        for extension in &self.extensions {
            if !relative(&extension.path)
                || !digest(&extension.sha256)
                || !declared.insert(&extension.path)
                || !baseline
                    .files
                    .iter()
                    .any(|file| file.role == FileRole::Runtime && file.path == extension.path)
            {
                return Err(error(
                    "invalid, duplicate or non-runtime extension path/hash",
                ));
            }
        }
        let mut changed = BTreeSet::new();
        for file in baseline
            .files
            .iter()
            .filter(|file| file.role == FileRole::Runtime)
        {
            let oracle = root.join("tests/oracles/pr14").join(&file.path);
            if codec::digest(&fs::read(&oracle)?) != file.sha256 {
                return Err(error(format!(
                    "PR14 oracle baseline changed: {}",
                    file.path.display()
                )));
            }
            let current = codec::digest(&fs::read(root.join(&file.path))?);
            if current != file.sha256 {
                changed.insert(&file.path);
                if !self
                    .extensions
                    .iter()
                    .any(|extension| extension.path == file.path && extension.sha256 == current)
                {
                    return Err(error(format!(
                        "unapproved runtime extension hash: {}",
                        file.path.display()
                    )));
                }
            }
        }
        if changed != declared {
            return Err(error(
                "runtime extension scope differs from changed runtime paths",
            ));
        }
        for gate in &self.gates {
            if !suite.cases.iter().any(|case| matches!(&case.execution, Execution::Nix { installable } if installable == gate)) {
                return Err(error(format!("runtime extension gate unregistered: {gate}")));
            }
        }
        for id in &self.qualified_cases {
            let case = suite
                .cases
                .iter()
                .find(|case| &case.id == id)
                .ok_or_else(|| error(format!("runtime extension case unregistered: {id}")))?;
            let oracle = id == "nix.x86_64-linux.postgres-lifecycle-oracle-test";
            let prefix = if oracle { "nix." } else { "vm." };
            if case.profile != (if oracle { Profile::Full } else { Profile::Vm })
                || !matches!(&case.execution, Execution::Nix { installable } if self.gates.contains(installable)
                    && installable == &format!(".#checks.{}", id.strip_prefix(prefix).unwrap_or("")))
                || !case.artifact_specs.iter().any(|artifact| {
                    artifact.required && matches!(artifact.kind, ArtifactKind::NixOutputs)
                })
                || !case.artifact_specs.iter().any(|artifact| {
                    artifact.required && matches!(artifact.kind, ArtifactKind::Junit)
                })
            {
                return Err(error(format!(
                    "runtime extension case lacks bound gate/artifacts: {id}"
                )));
            }
            if !case.artifact_specs.iter().any(|artifact| {
                artifact.required && matches!(artifact.kind, ArtifactKind::Semantic)
            }) {
                return Err(error(format!(
                    "runtime extension case lacks required semantic artifacts: {id}"
                )));
            }
        }
        Ok(())
    }
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
        let extensions = match fs::read_to_string(root.join("tests/runtime-extensions.toml")) {
            Ok(text) => Some(toml::from_str::<RuntimeExtensions>(&text)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        if let Some(extensions) = &extensions {
            extensions.validate(self, root, suite)?;
        } else {
            for file in self.files.iter().filter(|f| f.role == FileRole::Runtime) {
                if codec::digest(&fs::read(root.join(&file.path))?) != file.sha256 {
                    return Err(error(format!(
                        "PR14 Python runtime baseline changed: {}",
                        file.path.display()
                    )));
                }
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
        for gate in self
            .gates
            .iter()
            .chain(extensions.iter().flat_map(|extension| &extension.gates))
        {
            if !ci.iter().any(|value| value.as_str() == Some(gate)) {
                return Err(error(format!(
                    "PR14 qualification gate removed from CI: {gate}"
                )));
            }
        }
        Ok(())
    }
}
