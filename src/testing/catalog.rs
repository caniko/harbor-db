//! Strict, static suite inventory. Loading and selecting never execute a case or evaluate Nix.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Fast,
    Integration,
    Vm,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Maturity {
    Prototype,
    Crystallized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dimension {
    Positive,
    Rejection,
    Repetition,
    Concurrency,
    Interruption,
    Recovery,
    Compatibility,
}

impl Dimension {
    pub const ALL: [Self; 7] = [
        Self::Positive,
        Self::Rejection,
        Self::Repetition,
        Self::Concurrency,
        Self::Interruption,
        Self::Recovery,
        Self::Compatibility,
    ];
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub version: u32,
    pub capabilities: Vec<Capability>,
    pub cases: Vec<Case>,
    #[serde(default)]
    pub exclusions: Vec<TestExclusion>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestExclusion {
    pub source: String,
    pub selector: String,
    pub reason: String,
}

// Wire defaults keep the inventory readable; callers always receive complete cases.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseDefaults {
    profile: Option<Profile>,
    maturity: Option<Maturity>,
    deadline_seconds: Option<u64>,
    resources: Option<Resources>,
    platforms: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    artifacts: Option<Vec<String>>,
    coverage_note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSuite {
    version: u32,
    capabilities: Vec<Capability>,
    #[serde(default)]
    defaults: CaseDefaults,
    #[serde(default)]
    cases: Vec<WireCase>,
    #[serde(default)]
    inventories: Vec<Inventory>,
    #[serde(default)]
    coverage: Vec<CoverageBinding>,
    #[serde(default)]
    nix_inventories: Vec<NixInventory>,
    #[serde(default)]
    exclusions: Vec<TestExclusion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum NixNamespace {
    Checks,
    Packages,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NixInventory {
    namespace: NixNamespace,
    id_prefix: String,
    source: String,
    systems: Vec<String>,
    names: Vec<String>,
    profile: Profile,
    deadline_seconds: u64,
    resources: Resources,
    coverage_note: String,
}

impl NixInventory {
    fn expand(self, defaults: &CaseDefaults) -> Result<Vec<Case>, CatalogError> {
        unique(
            self.systems.iter().map(String::as_str),
            "Nix inventory system",
        )?;
        unique(self.names.iter().map(String::as_str), "Nix inventory name")?;
        nonempty(&self.id_prefix, "Nix inventory ID prefix")?;
        if self.systems.is_empty() || self.names.is_empty() {
            return Err(invalid("empty Nix inventory"));
        }
        let namespace = match self.namespace {
            NixNamespace::Checks => "checks",
            NixNamespace::Packages => "packages",
        };
        let mut cases = Vec::new();
        for system in self.systems {
            for name in &self.names {
                if !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return Err(invalid("invalid Nix inventory name"));
                }
                cases.push(
                    WireCase {
                        id: format!("{}{}.{}", self.id_prefix, system, name),
                        source: format!("{}::{namespace}.{name}", self.source),
                        execution: Execution::Nix {
                            installable: format!(".#{}.{system}.{name}", namespace),
                        },
                        profile: Some(self.profile),
                        maturity: Some(Maturity::Prototype),
                        deadline_seconds: Some(self.deadline_seconds),
                        resources: Some(self.resources.clone()),
                        platforms: Some(vec![system.clone()]),
                        depends_on: Some(Vec::new()),
                        artifacts: None,
                        artifact_specs: Vec::new(),
                        python_migration_id: None,
                        coverage: Vec::new(),
                        coverage_note: Some(self.coverage_note.clone()),
                    }
                    .resolve(defaults)?,
                );
            }
        }
        Ok(cases)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Origin {
    Python,
    Rust,
}

/// Compact spelling of individual cases, never a discovery command or aggregate test.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    origin: Origin,
    id_prefix: String,
    selector_prefix: String,
    source: String,
    argv_prefix: Vec<String>,
    argv_suffix: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    selectors: Vec<String>,
    profile: Option<Profile>,
    maturity: Option<Maturity>,
    deadline_seconds: Option<u64>,
    resources: Option<Resources>,
    platforms: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    artifacts: Option<Vec<String>>,
    coverage_note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoverageBinding {
    case: String,
    capability: String,
    operation: String,
    dimension: Dimension,
    evidence: String,
}

impl Inventory {
    fn expand(self, defaults: &CaseDefaults) -> Result<Vec<Case>, CatalogError> {
        nonempty(&self.id_prefix, "inventory ID prefix")?;
        nonempty(&self.source, "inventory source")?;
        unique(
            self.selectors.iter().map(String::as_str),
            "inventory selector",
        )?;
        if self.selectors.is_empty() || self.argv_prefix.is_empty() {
            return Err(invalid("empty inventory selectors or argv prefix"));
        }
        self.selectors
            .into_iter()
            .map(|selector| {
                let selector = format!("{}{selector}", self.selector_prefix);
                let mut argv = self.argv_prefix.clone();
                argv.push(selector.clone());
                argv.extend(self.argv_suffix.clone());
                WireCase {
                    id: format!("{}{selector}", self.id_prefix),
                    source: format!("{}::{selector}", self.source),
                    execution: Execution::Argv {
                        argv,
                        env: self.env.clone(),
                    },
                    profile: self.profile,
                    maturity: self.maturity,
                    deadline_seconds: self.deadline_seconds,
                    resources: self.resources.clone(),
                    platforms: self.platforms.clone(),
                    depends_on: self.depends_on.clone(),
                    artifacts: self.artifacts.clone(),
                    artifact_specs: Vec::new(),
                    python_migration_id: matches!(self.origin, Origin::Python).then_some(selector),
                    coverage: Vec::new(),
                    coverage_note: self.coverage_note.clone(),
                }
                .resolve(defaults)
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCase {
    id: String,
    source: String,
    execution: Execution,
    profile: Option<Profile>,
    maturity: Option<Maturity>,
    deadline_seconds: Option<u64>,
    resources: Option<Resources>,
    platforms: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    artifacts: Option<Vec<String>>,
    #[serde(default)]
    artifact_specs: Vec<ArtifactDescriptor>,
    python_migration_id: Option<String>,
    #[serde(default)]
    coverage: Vec<CoverageClaim>,
    coverage_note: Option<String>,
}

impl WireCase {
    fn resolve(self, defaults: &CaseDefaults) -> Result<Case, CatalogError> {
        fn required<T>(value: Option<T>, field: &str, id: &str) -> Result<T, CatalogError> {
            value.ok_or_else(|| invalid(format!("missing {field} for {id}")))
        }
        let mut artifact_specs = self.artifact_specs;
        if matches!(self.execution, Execution::Nix { .. }) && artifact_specs.is_empty() {
            artifact_specs.push(ArtifactDescriptor {
                version: 1,
                source: self.id.clone(),
                path: "nix-outputs.json".into(),
                kind: super::evidence::ArtifactKind::NixOutputs,
                required: true,
                sha256: None,
            });
        }
        if matches!(self.execution, Execution::Nix { .. })
            && self.profile.or(defaults.profile) == Some(Profile::Vm)
            && !artifact_specs.iter().any(|artifact| {
                matches!(artifact.kind, super::evidence::ArtifactKind::Junit) && artifact.required
            })
        {
            artifact_specs.push(ArtifactDescriptor {
                version: 1,
                source: self.id.clone(),
                path: "junit.xml".into(),
                kind: super::evidence::ArtifactKind::Junit,
                required: true,
                sha256: None,
            });
        }
        Ok(Case {
            profile: required(self.profile.or(defaults.profile), "profile", &self.id)?,
            maturity: required(self.maturity.or(defaults.maturity), "maturity", &self.id)?,
            deadline_seconds: required(
                self.deadline_seconds.or(defaults.deadline_seconds),
                "deadline_seconds",
                &self.id,
            )?,
            resources: required(
                self.resources.or_else(|| defaults.resources.clone()),
                "resources",
                &self.id,
            )?,
            platforms: required(
                self.platforms.or_else(|| defaults.platforms.clone()),
                "platforms",
                &self.id,
            )?,
            depends_on: required(
                self.depends_on.or_else(|| defaults.depends_on.clone()),
                "depends_on",
                &self.id,
            )?,
            artifacts: required(
                self.artifacts.or_else(|| defaults.artifacts.clone()),
                "artifacts",
                &self.id,
            )?,
            artifact_specs,
            coverage_note: self
                .coverage_note
                .or_else(|| defaults.coverage_note.clone()),
            id: self.id,
            source: self.source,
            execution: self.execution,
            python_migration_id: self.python_migration_id,
            coverage: self.coverage,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub operations: Vec<String>,
    pub required: Vec<Dimension>,
    #[serde(default)]
    pub not_applicable: Vec<NotApplicable>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotApplicable {
    /// None applies to every operation in the capability.
    #[serde(default)]
    pub operation: Option<String>,
    pub dimension: Dimension,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub profile: Profile,
    pub maturity: Maturity,
    pub source: String,
    pub execution: Execution,
    pub deadline_seconds: u64,
    pub resources: Resources,
    pub platforms: Vec<String>,
    pub depends_on: Vec<String>,
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub artifact_specs: Vec<ArtifactDescriptor>,
    #[serde(default)]
    pub python_migration_id: Option<String>,
    #[serde(default)]
    pub coverage: Vec<CoverageClaim>,
    /// Explains inventory that has not yet been reviewed into capability evidence.
    #[serde(default)]
    pub coverage_note: Option<String>,
}

/// Evidence paths are relative to a private per-case workspace; hashes are optional
/// expected hashes, not invented digests of artifacts that have not been produced.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub version: u32,
    pub source: String,
    pub path: PathBuf,
    pub kind: super::evidence::ArtifactKind,
    pub required: bool,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Case {
    pub fn resolved_artifacts(
        &self,
        workspace: &Path,
    ) -> Result<Vec<super::evidence::ArtifactSpec>, CatalogError> {
        if !workspace.is_absolute()
            || workspace
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(invalid(
                "artifact workspace must be an absolute normalized path",
            ));
        }
        self.artifact_specs
            .iter()
            .map(|artifact| {
                artifact.validate(&self.id)?;
                Ok(super::evidence::ArtifactSpec {
                    source: artifact.source.clone(),
                    path: workspace.join(&artifact.path),
                    kind: artifact.kind.clone(),
                    required: artifact.required,
                    sha256: artifact.sha256.clone(),
                })
            })
            .collect()
    }
}

impl ArtifactDescriptor {
    fn validate(&self, case: &str) -> Result<(), CatalogError> {
        if self.version != 1 || self.source != case {
            return Err(invalid("artifact version or source does not bind its case"));
        }
        if self.path.as_os_str().is_empty()
            || self
                .path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid("artifact path must be relative without traversal"));
        }
        if self.sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) {
            return Err(invalid(
                "artifact sha256 must be 64 lowercase hexadecimal digits",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Execution {
    Argv {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Nix {
        installable: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu: u32,
    pub memory_mib: u64,
    pub disk_mib: u64,
    pub exclusive: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageClaim {
    pub capability: String,
    pub operation: String,
    pub dimension: Dimension,
    pub evidence: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("could not read suite: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid suite TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid suite: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CoverageStatus {
    Missing,
    Prototype,
    Crystallized,
    NotApplicable { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageCell {
    pub capability: String,
    pub operation: String,
    pub dimension: Dimension,
    pub status: CoverageStatus,
    pub cases: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub profile: Profile,
    pub cells: Vec<CoverageCell>,
}

impl CoverageReport {
    /// Static completeness only; this does not imply any execution has passed.
    pub fn is_complete(&self) -> bool {
        self.cells.iter().all(|cell| {
            matches!(
                cell.status,
                CoverageStatus::Prototype
                    | CoverageStatus::Crystallized
                    | CoverageStatus::NotApplicable { .. }
            )
        })
    }

    /// Promotion completeness, independently of static coverage and execution success.
    pub fn is_crystallized(&self) -> bool {
        self.cells.iter().all(|cell| {
            matches!(
                cell.status,
                CoverageStatus::Crystallized | CoverageStatus::NotApplicable { .. }
            )
        })
    }
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid(message.into())
}

fn nonempty(value: &str, context: &str) -> Result<(), CatalogError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(invalid(format!("empty or NUL {context}")));
    }
    Ok(())
}

fn unique<'a>(
    values: impl IntoIterator<Item = &'a str>,
    context: &str,
) -> Result<(), CatalogError> {
    let mut seen = BTreeSet::new();
    for value in values {
        nonempty(value, context)?;
        if !seen.insert(value) {
            return Err(invalid(format!("duplicate {context}: {value}")));
        }
    }
    Ok(())
}

impl Suite {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn parse(input: &str) -> Result<Self, CatalogError> {
        let wire: WireSuite = toml::from_str(input)?;
        let mut suite = Self {
            version: wire.version,
            capabilities: wire.capabilities,
            cases: wire
                .cases
                .into_iter()
                .map(|case| case.resolve(&wire.defaults))
                .collect::<Result<_, _>>()?,
            exclusions: wire.exclusions,
        };
        for inventory in wire.inventories {
            suite.cases.extend(inventory.expand(&wire.defaults)?);
        }
        for inventory in wire.nix_inventories {
            suite.cases.extend(inventory.expand(&wire.defaults)?);
        }
        for binding in wire.coverage {
            let case = suite
                .cases
                .iter_mut()
                .find(|case| case.id == binding.case)
                .ok_or_else(|| invalid(format!("unknown coverage case {}", binding.case)))?;
            case.coverage.push(CoverageClaim {
                capability: binding.capability,
                operation: binding.operation,
                dimension: binding.dimension,
                evidence: binding.evidence,
            });
        }
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.version != 1 {
            return Err(invalid("unsupported catalog version"));
        }
        if self.capabilities.is_empty() || self.cases.is_empty() {
            return Err(invalid("empty capabilities or cases"));
        }
        unique(
            self.capabilities.iter().map(|cap| cap.id.as_str()),
            "capability ID",
        )?;
        unique(self.cases.iter().map(|case| case.id.as_str()), "case ID")?;
        unique(
            self.capabilities
                .iter()
                .flat_map(|cap| cap.operations.iter().map(String::as_str)),
            "operation",
        )?;
        unique(
            self.cases
                .iter()
                .filter_map(|case| case.python_migration_id.as_deref()),
            "Python migration ID",
        )?;
        let mut exclusions = BTreeSet::new();
        for exclusion in &self.exclusions {
            nonempty(&exclusion.source, "excluded test source")?;
            nonempty(&exclusion.selector, "excluded test selector")?;
            nonempty(&exclusion.reason, "excluded test reason")?;
            if !exclusions.insert((&exclusion.source, &exclusion.selector)) {
                return Err(invalid("duplicate test exclusion"));
            }
            if self
                .cases
                .iter()
                .any(|case| case.source == format!("{}::{}", exclusion.source, exclusion.selector))
            {
                return Err(invalid("test is both registered and excluded"));
            }
        }
        let capabilities: BTreeMap<_, _> = self
            .capabilities
            .iter()
            .map(|cap| (cap.id.as_str(), cap))
            .collect();
        let cases: BTreeMap<_, _> = self
            .cases
            .iter()
            .map(|case| (case.id.as_str(), case))
            .collect();
        for cap in &self.capabilities {
            if cap.operations.is_empty() {
                return Err(invalid(format!("no operations for {}", cap.id)));
            }
            if cap.required.len() != Dimension::ALL.len()
                || cap.required.iter().copied().collect::<BTreeSet<_>>()
                    != Dimension::ALL.into_iter().collect()
            {
                return Err(invalid(format!(
                    "{} must declare all seven dimensions; use explicit N/A reasons",
                    cap.id
                )));
            }
            let mut exemptions = BTreeSet::new();
            for na in &cap.not_applicable {
                nonempty(&na.reason, "N/A reason")?;
                if let Some(op) = &na.operation
                    && !cap.operations.contains(op)
                {
                    return Err(invalid(format!("unknown N/A operation {op}")));
                }
                for op in &cap.operations {
                    if na.operation.as_ref().is_none_or(|value| value == op)
                        && !exemptions.insert((op, na.dimension))
                    {
                        return Err(invalid(format!("duplicate N/A cell in {}", cap.id)));
                    }
                }
            }
        }
        for case in &self.cases {
            nonempty(&case.source, "source")?;
            if case.deadline_seconds == 0 || case.deadline_seconds > 86_400 {
                return Err(invalid(format!("unbounded deadline for {}", case.id)));
            }
            if case.resources.cpu == 0
                || case.resources.memory_mib == 0
                || case.resources.disk_mib == 0
            {
                return Err(invalid(format!("zero resource budget for {}", case.id)));
            }
            unique(
                case.resources.exclusive.iter().map(String::as_str),
                "exclusive resource",
            )?;
            unique(case.platforms.iter().map(String::as_str), "platform")?;
            if case.platforms.is_empty()
                || case
                    .platforms
                    .iter()
                    .any(|p| !matches!(p.as_str(), "x86_64-linux" | "aarch64-linux"))
            {
                return Err(invalid(format!(
                    "unknown or empty platforms for {}",
                    case.id
                )));
            }
            unique(case.artifacts.iter().map(String::as_str), "artifact")?;
            let mut artifact_paths = BTreeSet::new();
            for artifact in &case.artifact_specs {
                artifact.validate(&case.id)?;
                if !artifact_paths.insert(&artifact.path) {
                    return Err(invalid("duplicate artifact path"));
                }
            }
            unique(case.depends_on.iter().map(String::as_str), "dependency")?;
            match &case.execution {
                Execution::Argv { argv, env } => {
                    if argv.is_empty() {
                        return Err(invalid(format!("empty argv for {}", case.id)));
                    }
                    nonempty(&argv[0], "executable")?;
                    if argv.iter().any(|arg| arg.contains('\0')) {
                        return Err(invalid("NUL in argv"));
                    }
                    for (key, value) in env {
                        nonempty(key, "environment key")?;
                        if key.contains('=') || value.contains('\0') {
                            return Err(invalid("invalid environment entry"));
                        }
                    }
                }
                Execution::Nix { installable } => {
                    nonempty(installable, "Nix installable")?;
                    if !(installable.starts_with(".#checks.")
                        || installable.starts_with(".#packages."))
                        || installable.chars().any(char::is_whitespace)
                    {
                        return Err(invalid("expected local checks/packages Nix installable"));
                    }
                }
            }
            for dependency in &case.depends_on {
                let dependency = cases
                    .get(dependency.as_str())
                    .ok_or_else(|| invalid(format!("unknown dependency {dependency}")))?;
                if dependency.profile > case.profile {
                    return Err(invalid(format!(
                        "dependency {} is outside {} profile",
                        dependency.id, case.id
                    )));
                }
            }
            if let Some(note) = &case.coverage_note {
                nonempty(note, "coverage note")?;
            }
            if case.coverage.is_empty() && case.coverage_note.is_none() {
                return Err(invalid(format!(
                    "{} needs evidence or a coverage note",
                    case.id
                )));
            }
            let mut claims = BTreeSet::new();
            for claim in &case.coverage {
                let cap = capabilities
                    .get(claim.capability.as_str())
                    .ok_or_else(|| invalid(format!("unknown capability {}", claim.capability)))?;
                if !cap.operations.contains(&claim.operation) {
                    return Err(invalid(format!("unknown operation {}", claim.operation)));
                }
                nonempty(&claim.evidence, "coverage evidence")?;
                if !claims.insert((&claim.capability, &claim.operation, claim.dimension)) {
                    return Err(invalid(format!("duplicate coverage cell in {}", case.id)));
                }
                if cap.not_applicable.iter().any(|na| {
                    na.dimension == claim.dimension
                        && na
                            .operation
                            .as_ref()
                            .is_none_or(|op| op == &claim.operation)
                }) {
                    return Err(invalid("coverage contradicts N/A"));
                }
            }
        }
        fn visit<'a>(
            id: &'a str,
            cases: &BTreeMap<&'a str, &'a Case>,
            active: &mut BTreeSet<&'a str>,
            done: &mut BTreeSet<&'a str>,
        ) -> Result<(), CatalogError> {
            if done.contains(id) {
                return Ok(());
            }
            if !active.insert(id) {
                return Err(invalid(format!("dependency cycle at {id}")));
            }
            for dependency in &cases[id].depends_on {
                visit(dependency, cases, active, done)?;
            }
            active.remove(id);
            done.insert(id);
            Ok(())
        }
        let mut done = BTreeSet::new();
        for id in cases.keys() {
            visit(id, &cases, &mut BTreeSet::new(), &mut done)?;
        }
        Ok(())
    }

    /// Cumulative selection, stably ordered with dependencies before dependents.
    /// Load/parse validate dependency references and profile closure before selection.
    pub fn select(&self, profile: Profile) -> Vec<&Case> {
        fn visit<'a>(
            case: &'a Case,
            cases: &BTreeMap<&str, &'a Case>,
            seen: &mut BTreeSet<&'a str>,
            ordered: &mut Vec<&'a Case>,
        ) {
            if !seen.insert(&case.id) {
                return;
            }
            for dependency in &case.depends_on {
                if let Some(dependency) = cases.get(dependency.as_str()) {
                    visit(dependency, cases, seen, ordered);
                }
            }
            ordered.push(case);
        }
        let cases = self
            .cases
            .iter()
            .filter(|case| case.profile <= profile)
            .map(|case| (case.id.as_str(), case))
            .collect::<BTreeMap<_, _>>();
        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        for case in self.cases.iter().filter(|case| case.profile <= profile) {
            visit(case, &cases, &mut seen, &mut ordered);
        }
        ordered
    }

    pub fn coverage(&self, profile: Profile) -> CoverageReport {
        let selected = self.select(profile);
        let mut cells = Vec::new();
        for cap in &self.capabilities {
            for operation in &cap.operations {
                for dimension in &cap.required {
                    let evidence: Vec<_> = selected
                        .iter()
                        .copied()
                        .filter(|case| {
                            case.coverage.iter().any(|claim| {
                                claim.capability == cap.id
                                    && claim.operation == *operation
                                    && claim.dimension == *dimension
                            })
                        })
                        .collect();
                    let status = if let Some(na) = cap.not_applicable.iter().find(|na| {
                        na.dimension == *dimension
                            && na.operation.as_ref().is_none_or(|op| op == operation)
                    }) {
                        CoverageStatus::NotApplicable {
                            reason: na.reason.clone(),
                        }
                    } else if evidence
                        .iter()
                        .any(|case| case.maturity == Maturity::Crystallized)
                    {
                        CoverageStatus::Crystallized
                    } else if !evidence.is_empty() {
                        CoverageStatus::Prototype
                    } else {
                        CoverageStatus::Missing
                    };
                    cells.push(CoverageCell {
                        capability: cap.id.clone(),
                        operation: operation.clone(),
                        dimension: *dimension,
                        status,
                        cases: evidence.iter().map(|case| case.id.clone()).collect(),
                    });
                }
            }
        }
        CoverageReport { profile, cells }
    }
}
