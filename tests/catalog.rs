#![cfg(feature = "testing")]
use harbor_db::testing::catalog::{CoverageStatus, Dimension, Profile, Suite};

const FIXTURE: &str = r#"
version = 1
[[capabilities]]
id = "plan"
operations = ["harbor-db.validate"]
required = ["positive", "rejection", "repetition", "concurrency", "interruption", "recovery", "compatibility"]
[[capabilities.not_applicable]]
dimension = "concurrency"
reason = "Pure validation has no shared mutable state"
[[cases]]
id = "validate"
profile = "fast"
maturity = "crystallized"
source = "src/lib.rs:692"
deadline_seconds = 30
platforms = ["x86_64-linux"]
depends_on = []
artifacts = ["stdout", "stderr"]
resources = { cpu = 1, memory_mib = 128, disk_mib = 16, exclusive = [] }
execution = { kind = "argv", argv = ["cargo", "test", "validates_duplicate_and_missing_operations"] }
[[cases.coverage]]
capability = "plan"
operation = "harbor-db.validate"
dimension = "rejection"
evidence = "Rejects duplicate and missing operation identifiers"
"#;

#[test]
fn reports_missing_and_not_applicable_without_claiming_acceptance() {
    let suite = Suite::parse(FIXTURE).unwrap();
    let report = suite.coverage(Profile::Full);
    assert!(!report.is_complete());
    assert_eq!(report.cells.len(), 7);
    assert!(report.cells.iter().any(
        |cell| cell.dimension == Dimension::Positive && cell.status == CoverageStatus::Missing
    ));
    assert!(
        report
            .cells
            .iter()
            .any(|cell| cell.dimension == Dimension::Rejection
                && cell.status == CoverageStatus::Crystallized)
    );
    assert!(
        report
            .cells
            .iter()
            .any(|cell| cell.dimension == Dimension::Concurrency
                && matches!(cell.status, CoverageStatus::NotApplicable { .. }))
    );
}

#[test]
fn rejects_unknown_fields_enums_and_empty_commands() {
    for invalid in [
        FIXTURE.replace("version = 1", "version = 1\nsurprise = true"),
        FIXTURE.replace("profile = \"fast\"", "profile = \"quick\""),
        FIXTURE.replace("cpu = 1", "cpu = 1, surprise = true"),
        FIXTURE.replace(
            "[\"cargo\", \"test\", \"validates_duplicate_and_missing_operations\"]",
            "[]",
        ),
        FIXTURE.replace("deadline_seconds = 30", "deadline_seconds = 0"),
        FIXTURE.replace("Pure validation has no shared mutable state", ""),
    ] {
        assert!(Suite::parse(&invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn rejects_duplicate_ids_operations_unknown_references_and_cycles() {
    let suite = Suite::parse(FIXTURE).unwrap();
    let mut duplicate = suite.clone();
    duplicate.cases.push(duplicate.cases[0].clone());
    assert!(duplicate.validate().is_err());
    let mut duplicate_operation = suite.clone();
    duplicate_operation.capabilities[0]
        .operations
        .push("harbor-db.validate".into());
    assert!(duplicate_operation.validate().is_err());
    let mut unknown = suite.clone();
    unknown.cases[0].coverage[0].operation = "unknown".into();
    assert!(unknown.validate().is_err());
    let mut cycle = suite.clone();
    cycle.cases[0].depends_on.push("validate".into());
    assert!(cycle.validate().is_err());
    let mut unknown_dependency = suite;
    unknown_dependency.cases[0]
        .depends_on
        .push("unknown".into());
    assert!(unknown_dependency.validate().is_err());
}

#[test]
fn cumulative_profiles_and_prototype_evidence() {
    let mut suite = Suite::parse(FIXTURE).unwrap();
    let mut integration = suite.cases[0].clone();
    integration.id = "integration".into();
    integration.profile = Profile::Integration;
    integration.maturity = harbor_db::testing::catalog::Maturity::Prototype;
    integration.coverage[0].dimension = Dimension::Positive;
    suite.cases.push(integration);
    assert_eq!(suite.select(Profile::Fast).len(), 1);
    assert_eq!(suite.select(Profile::Integration).len(), 2);
    assert_eq!(suite.select(Profile::Vm).len(), 2);
    assert_eq!(suite.select(Profile::Full).len(), 2);
    assert!(
        suite
            .coverage(Profile::Full)
            .cells
            .iter()
            .any(|cell| cell.dimension == Dimension::Positive
                && cell.status == CoverageStatus::Prototype)
    );
    assert!(!suite.coverage(Profile::Full).is_complete());
}

#[test]
fn compact_inventory_materializes_one_exact_command_per_selector() {
    let input = format!(
        "{FIXTURE}\n[[inventories]]\norigin = \"python\"\nid_prefix = \"python.\"\nselector_prefix = \"test_example.Example.\"\nsource = \"tests/test_example.py\"\nargv_prefix = [\"python3\", \"-B\", \"-m\", \"unittest\"]\nargv_suffix = []\nselectors = [\"test_one\", \"test_two\"]\nprofile = \"integration\"\nmaturity = \"prototype\"\ndeadline_seconds = 60\nresources = {{ cpu = 1, memory_mib = 128, disk_mib = 16, exclusive = [] }}\nplatforms = [\"x86_64-linux\"]\ndepends_on = []\nartifacts = [\"stdout\", \"stderr\"]\ncoverage_note = \"Inventory only\"\n"
    );
    let suite = Suite::parse(&input).unwrap();
    assert_eq!(suite.cases.len(), 3);
    let case = &suite.cases[1];
    assert_eq!(
        case.python_migration_id.as_deref(),
        Some("test_example.Example.test_one")
    );
    match &case.execution {
        harbor_db::testing::catalog::Execution::Argv { argv, .. } => {
            assert_eq!(argv.last().unwrap(), "test_example.Example.test_one")
        }
        _ => panic!("expected executable inventory"),
    }
    assert!(Suite::parse(&input.replace("test_two", "test_one")).is_err());
}

#[test]
fn rejects_cross_case_cycles_na_overlap_and_profile_leaking_dependencies() {
    let suite = Suite::parse(FIXTURE).unwrap();
    let mut cycle = suite.clone();
    let mut second = cycle.cases[0].clone();
    second.id = "second".into();
    second.depends_on = vec!["validate".into()];
    cycle.cases[0].depends_on = vec!["second".into()];
    cycle.cases.push(second);
    assert!(cycle.validate().is_err());
    let mut overlap = suite.clone();
    let exemption = overlap.capabilities[0].not_applicable[0].clone();
    overlap.capabilities[0].not_applicable.push(exemption);
    assert!(overlap.validate().is_err());
    let mut leaking = suite.clone();
    let mut second = leaking.cases[0].clone();
    second.id = "vm".into();
    second.profile = Profile::Vm;
    leaking.cases[0].depends_on.push("vm".into());
    leaking.cases.push(second);
    assert!(leaking.validate().is_err());
    let mut contradictory = suite;
    contradictory.cases[0].coverage[0].dimension = Dimension::Concurrency;
    assert!(contradictory.validate().is_err());
}

#[test]
fn prototype_is_static_coverage_and_dependencies_are_selected_first() {
    let mut suite = Suite::parse(FIXTURE).unwrap();
    suite.cases[0].maturity = harbor_db::testing::catalog::Maturity::Prototype;
    let claim = suite.cases[0].coverage[0].clone();
    for dimension in Dimension::ALL {
        if !matches!(dimension, Dimension::Concurrency | Dimension::Rejection) {
            let mut additional = claim.clone();
            additional.dimension = dimension;
            suite.cases[0].coverage.push(additional);
        }
    }
    assert!(suite.coverage(Profile::Fast).is_complete());
    assert!(!suite.coverage(Profile::Fast).is_crystallized());
    let mut dependency = suite.cases[0].clone();
    dependency.id = "dependency".into();
    suite.cases[0].depends_on.push("dependency".into());
    suite.cases.push(dependency);
    suite.validate().unwrap();
    assert_eq!(suite.select(Profile::Fast)[0].id, "dependency");
}

#[test]
fn committed_inventory_is_individual_and_migration_bound() {
    let suite = Suite::load("tests/suite.toml").unwrap();
    assert_eq!(
        suite
            .cases
            .iter()
            .filter(|case| case.python_migration_id.is_some())
            .count(),
        174
    );
    assert_eq!(
        suite
            .cases
            .iter()
            .filter(|case| case.id.starts_with("rust."))
            .count(),
        20
    );
    assert!(!suite.coverage(Profile::Full).is_complete());
    assert!(suite.capabilities.iter().any(|capability| {
        capability
            .operations
            .iter()
            .any(|operation| operation == "harbor-db.restore")
    }));
    assert_eq!(
        suite
            .cases
            .iter()
            .filter(|case| case.id.starts_with("vm."))
            .count(),
        29
    );
    for profile in [
        Profile::Fast,
        Profile::Integration,
        Profile::Vm,
        Profile::Full,
    ] {
        let selected = suite.select(profile);
        let mut seen = std::collections::BTreeSet::new();
        for case in selected {
            for dependency in &case.depends_on {
                assert!(
                    seen.contains(dependency),
                    "{} before dependency {dependency}",
                    case.id
                );
            }
            seen.insert(case.id.clone());
        }
    }
}

#[test]
fn python_inventory_matches_source_methods_without_running_them() {
    let output = std::process::Command::new("python3")
        .args(["-B", "-c", r#"
import ast, json, pathlib
methods = []
for path in pathlib.Path('tests').glob('test_*.py'):
    for cls in ast.parse(path.read_text()).body:
        if isinstance(cls, ast.ClassDef):
            for method in cls.body:
                if isinstance(method, (ast.FunctionDef, ast.AsyncFunctionDef)) and method.name.startswith('test_'):
                    methods.append(f'{path.stem}.{cls.name}.{method.name}')
print(json.dumps(sorted(methods)))
"#])
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let existing: std::collections::BTreeSet<String> =
        serde_json::from_slice::<Vec<String>>(&output.stdout)
            .unwrap()
            .into_iter()
            .collect();
    let recorded = Suite::load("tests/suite.toml")
        .unwrap()
        .cases
        .into_iter()
        .filter_map(|case| case.python_migration_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(existing, recorded);
}

#[test]
fn inventories_are_strict_and_bindings_cannot_reference_missing_cases() {
    assert!(Suite::parse(&format!("{FIXTURE}\n[[coverage]]\ncase = \"missing\"\ncapability = \"plan\"\noperation = \"harbor-db.validate\"\ndimension = \"positive\"\nevidence = \"No such test\"\n")).is_err());
    let input = format!("{FIXTURE}\n[defaults]\nunknown = true\n");
    assert!(Suite::parse(&input).is_err());
    let mut suite = Suite::parse(FIXTURE).unwrap();
    suite.cases[0].coverage[0].capability = "missing".into();
    assert!(suite.validate().is_err());
    suite = Suite::parse(FIXTURE).unwrap();
    suite.capabilities[0].required.pop();
    assert!(suite.validate().is_err());
}

#[test]
fn artifact_descriptors_bind_case_and_reject_escape_or_invalid_hash() {
    let input = format!(
        "{FIXTURE}\n[[cases.artifact_specs]]\nversion = 1\nsource = \"validate\"\npath = \"semantic.json\"\nkind = \"semantic\"\nrequired = true\n"
    );
    let suite = Suite::parse(&input).unwrap();
    let specs = suite.cases[0]
        .resolved_artifacts(std::path::Path::new("/case-workspace"))
        .unwrap();
    assert_eq!(
        specs[0].path,
        std::path::Path::new("/case-workspace/semantic.json")
    );
    assert_eq!(specs[0].source, "validate");
    assert!(Suite::parse(&input.replace("semantic.json", "../escape.json")).is_err());
    assert!(Suite::parse(&input.replace("source = \"validate\"", "source = \"other\"")).is_err());
    assert!(Suite::parse(&format!("{input}\nsha256 = \"bad\"\n")).is_err());
    assert!(Suite::parse(&input.replace("version = 1\nsource", "version = 2\nsource")).is_err());
}

#[test]
fn native_inventory_matches_annotated_sources_and_explicit_fixture_exclusions() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("python3").current_dir(root)
        .args(["-B", "-c", r#"
import json, pathlib, re
pattern = re.compile(r'#\[(?:tokio::)?test(?:\([^\]]*\))?\](?:\s*#\[[^\]]*\])*\s*(?:async\s+)?fn\s+(\w+)')
sources = list(pathlib.Path('tests').rglob('*.rs'))
prefixes = {'src/planner/tests.rs': 'tests::',
            'src/bin/home-manager-backup.rs': 'tests::',
            'src/storage/recovery_capture.rs': 'storage::recovery_capture::tests::'}
sources += [pathlib.Path(path) for path in prefixes]
tests = []
for source in sources:
    prefix = prefixes.get(str(source), '')
    text = source.read_text()
    # Disposable crates embedded in Rust strings are fixtures, not selectors in
    # this test target. Remove literals before finding annotated declarations.
    text = re.sub(r'(?s)\b(?:b)?r(\#*)".*?"\1', '""', text)
    text = re.sub(r'"(?:\\.|[^"\\])*"', '""', text)
    text = re.sub(r'//[^\n]*|/\*.*?\*/', '', text, flags=re.S)
    tests.extend(f'{source}::{prefix}{name}' for name in pattern.findall(text))
print(json.dumps(sorted(tests)))
"#]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source: std::collections::BTreeSet<String> =
        serde_json::from_slice::<Vec<String>>(&output.stdout)
            .unwrap()
            .into_iter()
            .collect();
    let suite = Suite::load(root.join("tests/suite.toml")).unwrap();
    let mut registered = suite.cases.iter().filter(|case| matches!(&case.execution,
        harbor_db::testing::catalog::Execution::Argv { argv, .. } if argv.first().is_some_and(|arg| arg == "cargo")))
        .map(|case| case.source.clone()).collect::<std::collections::BTreeSet<_>>();
    registered.extend(
        suite
            .exclusions
            .iter()
            .map(|excluded| format!("{}::{}", excluded.source, excluded.selector)),
    );
    let missing = source.difference(&registered).collect::<Vec<_>>();
    let stale = registered.difference(&source).collect::<Vec<_>>();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "Unregistered Rust selectors: {missing:?}; stale selectors/exclusions: {stale:?}"
    );
}

#[test]
fn nix_inventory_matches_declared_exports_without_evaluation() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("python3").current_dir(root)
        .args(["-B", "-c", r#"
import json, pathlib, re
flake = pathlib.Path('flake.nix').read_text()
packages = flake.split('    packages = forAllSystems', 1)[1].split('    checks = forAllSystems', 1)[0]
checks = flake.split('    checks = forAllSystems', 1)[1].split('    formatter =', 1)[0]
packages = re.split(r'\bin\s*\{', packages, maxsplit=1)[1]
checks = re.split(r'\bin\s*\{', checks, maxsplit=1)[1]
def names(body):
    # Formatting may put `in` and `{` on separate lines and reindent the
    # optionalAttrs operands. Only the shallowest declarations are exports.
    declarations = re.findall(r'^( +)([a-zA-Z0-9_-]+)\s*=', body, re.M)
    depth = min(len(indent) for indent, _ in declarations)
    result = {name for indent, name in declarations if len(indent) == depth}
    for indent, inherited in re.findall(r'^( +)inherit ([a-zA-Z0-9_ -]+);', body, re.M):
        if len(indent) == depth: result.update(inherited.split())
    return result
package_names = names(packages)
both, native = re.split(r'\}\s*//\s*pkgs\.lib\.optionalAttrs\s*\(pkgs\.system == "x86_64-linux"\)\s*\{', checks, maxsplit=1)
check_names = names(both)
native_names = names(native)
exports = [f'.#packages.{system}.{name}' for system in ('x86_64-linux', 'aarch64-linux') for name in package_names]
exports += [f'.#checks.{system}.{name}' for system in ('x86_64-linux', 'aarch64-linux') for name in check_names]
exports += [f'.#checks.x86_64-linux.{name}' for name in native_names]
print(json.dumps(sorted(exports)))
"#]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let declared = serde_json::from_slice::<Vec<String>>(&output.stdout)
        .unwrap()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let recorded = Suite::load(root.join("tests/suite.toml"))
        .unwrap()
        .cases
        .into_iter()
        .filter_map(|case| match case.execution {
            harbor_db::testing::catalog::Execution::Nix { installable } => Some(installable),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let missing = declared.difference(&recorded).collect::<Vec<_>>();
    let stale = recorded.difference(&declared).collect::<Vec<_>>();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "Unregistered Nix exports: {missing:?}; stale exports: {stale:?}"
    );
}
