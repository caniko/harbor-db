//! Re-exec fixture: the integration test process is an independent worker owner.
#[test]
#[ignore = "subprocess fixture invoked by supervisor integration tests"]
fn worker_process() {
    let run = std::env::var_os("HARBOR_SUPERVISOR_FIXTURE_RUN").expect("fixture run");
    harbor_db::testing::supervisor::worker(std::path::Path::new(&run)).unwrap();
}

#[test]
#[ignore = "subprocess fixture with isolated service-command PATH"]
fn detached_launch_failure() {
    let run = std::env::var_os("HARBOR_SUPERVISOR_FIXTURE_RUN").expect("fixture run");
    let run = std::path::Path::new(&run);
    let executable = std::env::current_exe().unwrap();
    assert!(harbor_db::testing::supervisor::start_detached(run, &executable).is_err());
    let state = harbor_db::testing::supervisor::status(run).unwrap();
    assert!(state.terminal);
    assert_eq!(
        state.verification.verdict,
        harbor_db::testing::supervisor::Verdict::Failed
    );
    assert!(harbor_db::testing::supervisor::start_detached(run, &executable).is_err());
    assert!(harbor_db::testing::supervisor::worker(run).is_err());
}
