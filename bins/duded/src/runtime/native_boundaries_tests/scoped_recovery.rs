use super::*;
use crate::runtime::repository_hire_tests::{Runner, Setup};

fn scoped() -> (
    Setup,
    Daemon<MockNativeHost, Runner, common::TestClock, common::FixtureRng>,
    AgentId,
) {
    let setup = Setup::with_repositories(&["web", "api"]);
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    let host = MockNativeHost::new();
    let seams = common::seams(
        &setup.settings,
        host,
        Arc::new(Runner::default()),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    let daemon = crate::boot(&setup.settings, seams).unwrap();
    (setup, daemon, agent)
}

#[test]
fn scoped_recovery_checks_every_root_and_requires_a_complete_census() {
    let (_setup, mut daemon, agent) = scoped();
    assert!(
        daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    let dudes_proto::repository::HireRepositories::Scoped(hire) =
        &daemon.sup.world().agents[&agent].repositories
    else {
        unreachable!()
    };
    let copy = hire
        .repositories
        .entries()
        .iter()
        .find(|copy| copy.repo_id != *hire.repositories.primary_id())
        .unwrap()
        .value
        .recorded
        .clone();
    let root = copy.execution.directory;
    let check = (GoalId::from_parts(81, 1), root.clone());
    daemon.ctx.predicate_folders.insert(check.clone(), 1);
    assert!(
        !daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    daemon.ctx.predicate_folders.remove(&check);
    assert!(
        daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    let moved = root.with_extension("preserved");
    std::fs::rename(&root, &moved).unwrap();
    assert!(
        !daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    std::fs::create_dir(&root).unwrap();
    assert!(
        !daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&moved, &root).unwrap();
    assert!(
        daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    for status in [
        OwnedTargetsStatus::UnknownUnsupported,
        OwnedTargetsStatus::Truncated { omitted: 1 },
    ] {
        daemon.ctx.host.state.lock().unwrap().query_status = Some(status);
        assert!(
            !daemon
                .ctx
                .recovery_consent_checkout_clear(&daemon.sup, agent)
        );
    }
    daemon.ctx.host.state.lock().unwrap().query_status = None;
    assert!(
        daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
}
