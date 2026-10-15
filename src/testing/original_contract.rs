//! Producer-owned original obligations for independent hosted selection.
//! Export is static admission, never execution or consumer/live qualification.
use super::{
    baseline::{Baseline, FileRole, SourceFile},
    catalog::Suite,
    evidence,
    supervisor::{Result, error},
};
use crate::storage::codec;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
pub struct OriginalMethod {
    /// Original source name, unchanged even when an extension uses a frozen oracle.
    pub source: PathBuf,
    /// Root-relative location of the actual retained original test bytes.
    pub oracle: PathBuf,
    pub oracle_sha256: String,
    /// Every enrolled catalog case associated with this original obligation.
    pub catalog_cases: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
pub struct OriginalContract {
    pub schema: &'static str,
    pub scope: &'static str,
    pub baseline_head: String,
    pub method_inventory_sha256: String,
    /// Hash of the complete resolved catalog, including commands and coverage bindings.
    pub catalog_sha256: String,
    pub methods: BTreeMap<String, OriginalMethod>,
    pub original_gates: BTreeSet<String>,
    /// Actual original runtime/test bytes to retain with the immutable cohort.
    pub oracle_files: BTreeMap<PathBuf, String>,
    pub executed: bool,
    pub runtime_qualified: bool,
}

fn oracle(root: &Path, file: &SourceFile) -> Result<PathBuf> {
    for relative in [
        file.path.clone(),
        Path::new("tests/oracles/pr14").join(&file.path),
    ] {
        let path = root.join(&relative);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file()
            && path.canonicalize()?.starts_with(root)
            && codec::digest(&evidence::bounded_read(&path)?) == file.sha256
        {
            return Ok(relative);
        }
    }
    Err(error(format!(
        "original oracle bytes missing or changed: {}",
        file.path.display()
    )))
}

impl Baseline {
    /// Export all original methods and gate obligations after existing baseline
    /// admission. Selection is independent of successful jobs and never executes
    /// tests, commands, Nix evaluation or a consumer artifact.
    pub fn original_contract(&self, root: &Path, suite: &Suite) -> Result<OriginalContract> {
        let root = root.canonicalize()?;
        self.validate_migration(&root, suite)?;
        let required = self.python_tests(&root)?;
        let mut oracle_files = BTreeMap::new();
        let mut tests = BTreeMap::new();
        for file in self
            .files
            .iter()
            .filter(|file| matches!(file.role, FileRole::Runtime | FileRole::Test))
        {
            let selected = oracle(&root, file)?;
            oracle_files.insert(selected.clone(), file.sha256.clone());
            if file.role == FileRole::Test {
                let module = file
                    .path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| error("invalid original test source"))?;
                tests.insert(module, (file, selected));
            }
        }
        let mut methods = BTreeMap::new();
        for selector in required {
            let module = selector
                .split('.')
                .next()
                .ok_or_else(|| error("invalid original selector"))?;
            let (file, selected) = tests
                .get(module)
                .ok_or_else(|| error("original selector source missing"))?;
            let catalog_cases = suite
                .cases
                .iter()
                .filter(|case| case.python_migration_id.as_deref() == Some(&selector))
                .map(|case| case.id.clone())
                .collect();
            methods.insert(
                selector,
                OriginalMethod {
                    source: file.path.clone(),
                    oracle: selected.clone(),
                    oracle_sha256: file.sha256.clone(),
                    catalog_cases,
                },
            );
        }
        Ok(OriginalContract {
            schema: "harbor-db.original-obligation-contract.v1",
            scope: "original-obligation-inventory",
            baseline_head: self.head.clone(),
            method_inventory_sha256: self.python_inventory_sha256.clone(),
            catalog_sha256: codec::digest(&serde_json::to_vec(suite)?),
            methods,
            original_gates: self.gates.iter().cloned().collect(),
            oracle_files,
            executed: false,
            runtime_qualified: false,
        })
    }
}
