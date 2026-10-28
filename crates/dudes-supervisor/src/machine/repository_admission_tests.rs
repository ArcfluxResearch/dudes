use super::*;
use dudes_proto::record::RecordedWorkspace;

#[test]
fn unresolved_current_repositories_refuse_issue_persistence_and_require_the_exact_hire() {
    let mut producer = crate::testing::Harness::new();
    producer.launch = false;
    let (agent, _) = producer.hire_with_goal(&["/usr/bin/true"]);
    let mut hired = producer.journal[0].clone();
    let Record::AgentHired { workspace, .. } = &mut hired.record else {
        unreachable!()
    };
    *workspace = Some(RecordedWorkspace {
        id: "service".into(),
        label: "Service".into(),
    });
    let issue = producer
        .journal
        .iter()
        .find(|entry| entry.record.turn_issue().is_some())
        .unwrap()
        .record
        .clone();
    let mut harness = crate::testing::Harness::new();
    harness.step(Input::Replayed {
        lsn: Lsn(1),
        entry: hired,
    });
    let machine = &mut harness.supervisor;
    assert!(!machine.repository_admitted(agent));
    machine.observe_repository_admission(crate::RepositoryAdmission::new([(agent, Lsn(1), true)]));
    assert!(machine.repository_admitted(agent));
    machine.observe_repository_admission(crate::RepositoryAdmission::new([(agent, Lsn(1), false)]));
    machine.persist_unobserved(Timestamp::UNIX_EPOCH, issue.clone(), Then::Fold);
    assert!(machine.take_effects().is_empty());
    assert!(machine.launch_held(agent, &crate::RecoveryOperation::empty()));
    machine.observe_repository_admission(crate::RepositoryAdmission::new([(agent, Lsn(2), true)]));
    assert!(
        !machine.repository_admitted(agent),
        "another hire position cannot authorize this one"
    );
    machine.observe_repository_admission(crate::RepositoryAdmission::new([(agent, Lsn(1), true)]));
    machine.persist_unobserved(Timestamp::UNIX_EPOCH, issue, Then::Fold);
    assert!(matches!(machine.effects(), [Effect::Persist { .. }]));
}
