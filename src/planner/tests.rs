use super::*;

fn command(program: &str) -> CommandSpec {
    CommandSpec::new(program, ["-c", "exit 0"])
}

fn operation(id: &str, depends_on: &[&str]) -> MigrationOperation {
    MigrationOperation {
        id: id.to_owned(),
        kind: OperationKind::Generic,
        lifecycle: Lifecycle::Ensure,
        backend: Backend::Generic,
        phase: Phase::Schema,
        safety: Safety::Automatic,
        apply: command("sh"),
        check: Some(command("sh")),
        depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn plan(operations: Vec<MigrationOperation>) -> MigrationPlan {
    MigrationPlan {
        version: PLAN_VERSION,
        name: "test".to_owned(),
        operations,
    }
}

#[test]
fn validates_duplicate_and_missing_operations() {
    let duplicate = plan(vec![operation("one", &[]), operation("one", &[])]);
    assert!(matches!(
        duplicate.validate(),
        Err(MigrationError::DuplicateOperation(_))
    ));

    let missing = plan(vec![operation("one", &["missing"])]);
    assert!(matches!(
        missing.validate(),
        Err(MigrationError::MissingDependency { .. })
    ));
}

#[test]
fn detects_dependency_cycles() {
    let cyclic = plan(vec![operation("one", &["two"]), operation("two", &["one"])]);
    assert!(matches!(
        cyclic.validate(),
        Err(MigrationError::DependencyCycle(_))
    ));
}

#[test]
fn structured_command_does_not_split_arguments() {
    let spec = CommandSpec::new("migration", ["--database-url", "postgres:///db?x=a b"]);
    assert_eq!(spec.args, vec!["--database-url", "postgres:///db?x=a b"]);
}

#[test]
fn serializes_clickhouse_backend_name_used_by_nix_plans() {
    assert_eq!(
        serde_json::to_string(&Backend::ClickHouse).expect("backend serializes"),
        "\"clickhouse\""
    );
}

#[test]
fn serializes_typedb_backend_name_used_by_nix_plans() {
    assert_eq!(
        serde_json::to_string(&Backend::Typedb).expect("backend serializes"),
        "\"typedb\""
    );
}

#[test]
fn typedb_operations_validate_and_keep_secrets_out_of_plans() {
    let mut operation = operation("schema", &[]);
    operation.backend = Backend::Typedb;
    operation.phase = Phase::Schema;
    operation.apply = CommandSpec::new("chaosbox", ["db", "migrate"]);
    operation
        .apply
        .credential_environment
        .insert("PASSWORD_FILE".to_owned(), "typedb-password".to_owned());
    operation.check = Some(CommandSpec::new("chaosbox", ["db", "check"]));
    let validated = plan(vec![operation]);
    validated.validate().expect("typedb plan validates");
    let serialized = serde_json::to_string(&validated).expect("plan serializes");
    assert!(serialized.contains("\"backend\":\"typedb\""));
    assert!(serialized.contains("\"typedb-password\""));
    assert!(!serialized.contains("super-secret-value"));
}

#[test]
fn generic_metadata_and_credential_refs_are_wire_safe() {
    let mut operation = operation("provision", &[]);
    operation.kind = OperationKind::Credential;
    operation.lifecycle = Lifecycle::Ensure;
    operation.apply.credential_args = vec!["password".to_owned()];
    operation
        .apply
        .credential_environment
        .insert("PASSWORD_FILE".to_owned(), "password".to_owned());
    let serialized = serde_json::to_string(&plan(vec![operation])).expect("plan serializes");

    assert!(serialized.contains("\"kind\":\"credential\""));
    assert!(serialized.contains("\"lifecycle\":\"ensure\""));
    assert!(serialized.contains("\"password\""));
    assert!(!serialized.contains("super-secret-value"));
    assert!(!serialized.contains("/run/secrets/password"));
}

#[test]
fn v1_database_shape_still_decodes_without_generic_metadata() {
    let old = r#"{
            "version": 1,
            "name": "legacy",
            "operations": [{
                "id": "schema",
                "backend": "postgres",
                "phase": "schema",
                "apply": {"program": "/bin/true"}
            }]
        }"#;
    let decoded: MigrationPlan = serde_json::from_str(old).expect("legacy plan decodes");
    decoded.validate().expect("legacy plan validates");
    assert_eq!(decoded.operations[0].kind, OperationKind::Database);
    assert_eq!(decoded.operations[0].lifecycle, Lifecycle::Ensure);
}

#[test]
fn credential_references_cannot_escape_the_systemd_directory() {
    let mut operation = operation("provision", &[]);
    operation.apply.credential_args = vec!["../password".to_owned()];

    assert!(matches!(
        plan(vec![operation]).validate(),
        Err(MigrationError::InvalidCredentialName { .. })
    ));
}

#[tokio::test]
async fn apply_runs_dependencies_before_selected_operation() {
    let mut child = operation("child", &["parent"]);
    child.apply = command("sh");
    let report = run_plan(
        &plan(vec![operation("parent", &[]), child]),
        RunMode::Apply,
        &RunOptions {
            operations: ["child".to_owned()].into_iter().collect(),
            confirm: false,
        },
    )
    .await
    .expect("plan runs");
    assert_eq!(
        report.operations,
        vec![
            ("parent".to_owned(), OperationStatus::Applied),
            ("child".to_owned(), OperationStatus::Applied),
        ]
    );
}

#[tokio::test]
async fn operator_operation_requires_selection_and_confirmation() {
    let mut operator = operation("operator", &[]);
    operator.safety = Safety::OperatorConfirmed;
    let plan = plan(vec![operator]);
    let skipped = run_plan(&plan, RunMode::Apply, &RunOptions::default())
        .await
        .expect("manual operation is skipped");
    assert_eq!(skipped.operations[0].1, OperationStatus::SkippedManual);

    let selected = RunOptions {
        operations: ["operator".to_owned()].into_iter().collect(),
        confirm: false,
    };
    assert!(matches!(
        run_plan(&plan, RunMode::Apply, &selected).await,
        Err(MigrationError::ConfirmationRequired(_))
    ));
}

#[tokio::test]
async fn check_preserves_pending_exit_semantics() {
    let mut pending = operation("pending", &[]);
    pending.check = Some(CommandSpec::new("sh", ["-c", "exit 2"]));
    let report = run_plan(&plan(vec![pending]), RunMode::Check, &RunOptions::default())
        .await
        .expect("pending checks are reports, not runner failures");
    assert_eq!(report.operations[0].1, OperationStatus::Pending);
    assert!(report.is_pending());
}

fn marked_command(mark: &std::path::Path, body: &str) -> CommandSpec {
    CommandSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), body.to_owned()],
        environment: [("MARK".to_owned(), mark.to_string_lossy().into_owned())]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn restore_repairs_pending_operations_and_verifies() {
    let mark = std::env::temp_dir().join(format!("harbor-db-restore-mark-{}", std::process::id()));
    let _ = std::fs::remove_file(&mark);
    let mut broken = operation("endpoint", &[]);
    broken.check = Some(marked_command(&mark, "test -e \"$MARK\" || exit 2"));
    broken.apply = marked_command(&mark, "touch \"$MARK\"");
    let report = run_plan(
        &plan(vec![broken]),
        RunMode::Restore,
        &RunOptions::default(),
    )
    .await
    .expect("restore repairs and verifies");
    assert_eq!(report.operations[0].1, OperationStatus::Restored);
    assert!(mark.exists());
    assert!(!report.is_pending());
    let _ = std::fs::remove_file(&mark);
}

#[tokio::test]
async fn restore_leaves_current_operations_untouched() {
    let mut current = operation("endpoint", &[]);
    current.apply = CommandSpec::new("sh", ["-c", "exit 1"]);
    let report = run_plan(
        &plan(vec![current]),
        RunMode::Restore,
        &RunOptions::default(),
    )
    .await
    .expect("current operations skip the apply command");
    assert_eq!(report.operations[0].1, OperationStatus::Current);
}

#[tokio::test]
async fn restore_reports_pending_when_repair_does_not_converge() {
    let mut broken = operation("endpoint", &[]);
    broken.check = Some(CommandSpec::new("sh", ["-c", "exit 2"]));
    let report = run_plan(
        &plan(vec![broken]),
        RunMode::Restore,
        &RunOptions::default(),
    )
    .await
    .expect("non-converging restores are reports, not runner failures");
    assert_eq!(report.operations[0].1, OperationStatus::Pending);
    assert!(report.is_pending());
}

#[tokio::test]
async fn restore_requires_checks_and_operator_confirmation() {
    let mut uncheckable = operation("apply-only", &[]);
    uncheckable.check = None;
    assert!(matches!(
        run_plan(
            &plan(vec![uncheckable]),
            RunMode::Restore,
            &RunOptions::default()
        )
        .await,
        Err(MigrationError::MissingCheckCommand(_))
    ));

    let mut operator = operation("operator", &[]);
    operator.safety = Safety::OperatorConfirmed;
    let skipped = run_plan(
        &plan(vec![operator.clone()]),
        RunMode::Restore,
        &RunOptions::default(),
    )
    .await
    .expect("unselected restore operations are skipped");
    assert_eq!(skipped.operations[0].1, OperationStatus::SkippedManual);

    let selected = RunOptions {
        operations: ["operator".to_owned()].into_iter().collect(),
        confirm: false,
    };
    assert!(matches!(
        run_plan(&plan(vec![operator]), RunMode::Restore, &selected).await,
        Err(MigrationError::ConfirmationRequired(_))
    ));
}
