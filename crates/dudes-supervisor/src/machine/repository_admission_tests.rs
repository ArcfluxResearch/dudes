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

#[test]
fn restored_repository_admission_retries_once_without_persisting_pause_or_resume() {
    let mut harness = crate::testing::Harness::new();
    let agent = harness.hire();
    let hire = harness.supervisor.world.agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, false)]));
    harness.set_goal(agent, &["/usr/bin/true"]);
    harness.drive(Input::Tick);
    assert!(
        harness
            .records()
            .all(|record| record.turn_issue().is_none())
    );
    assert!(!harness.supervisor.world.agents[&agent].paused);
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, true)]));
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);
    assert_eq!(
        harness
            .records()
            .filter(|record| record.turn_issue().is_some())
            .count(),
        1
    );
    assert_eq!(harness.host.spawned.len(), 1);
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::Pause { .. } | Record::Resume { .. }))
    );
}

#[test]
fn restored_admission_preserves_operator_controls_and_waits_for_their_acknowledgement() {
    let mut harness = crate::testing::Harness::new();
    let agent = harness.hire();
    let hire = harness.supervisor.world.agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, false)]));
    harness.set_goal(agent, &["/usr/bin/true"]);
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, true)]));
    harness.fsync = false;
    let pending = harness.step(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    let token = pending
        .iter()
        .find_map(|seen| match seen {
            crate::testing::Seen::Persist { token, .. } => Some(*token),
            _ => None,
        })
        .unwrap();
    harness.step(Input::Tick);
    assert!(harness.host.spawned.is_empty());
    let append = harness.persisted_append(token, Lsn(1000));
    harness.step(Input::Persisted { token, append });
    harness.fsync = true;
    harness.drive(Input::Tick);
    assert!(harness.supervisor.world.agents[&agent].paused);
    assert!(harness.host.spawned.is_empty());
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::Resume { .. }))
    );
}

#[test]
fn repository_admission_denies_hooks_without_consuming_notes_or_operator_controls() {
    let mut harness = crate::testing::Harness::new();
    let agent = harness.hire();
    let hire = harness.supervisor.world.agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, false)]));
    let before = harness.journal.clone();
    for kind in [
        EventKind::PromptSubmit,
        EventKind::PreTool {
            name: "Read".into(),
            input: serde_json::Value::Null,
        },
        EventKind::CompactStart,
    ] {
        let (reply, consumed) = harness.supervisor.gate_reply(agent, &kind);
        assert!(matches!(reply.decision, HookDecision::Deny { .. }));
        assert!(consumed.is_none());
    }
    assert_eq!(harness.journal, before);
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, true)]));
    let (reply, _) = harness
        .supervisor
        .gate_reply(agent, &EventKind::PromptSubmit);
    assert_eq!(reply.decision, HookDecision::Allow);
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    harness.drive(Input::Tick);
    assert!(harness.supervisor.world.agents[&agent].halted);
    let (reply, _) = harness
        .supervisor
        .gate_reply(agent, &EventKind::PromptSubmit);
    assert_eq!(reply.decision, HookDecision::Halt);
}

#[test]
fn restored_repository_admission_keeps_an_independent_automatic_hold() {
    let mut harness = crate::testing::Harness::new();
    let agent = harness.hire();
    let hire = harness.supervisor.world.agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, false)]));
    harness.set_goal(agent, &["/usr/bin/true"]);
    let cause = dudes_proto::AutomaticHold::LaunchRefused {
        reason: "independent refusal".into(),
    };
    harness.step(Input::Replayed {
        lsn: Lsn(1000),
        entry: Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::AgentHeld {
                agent,
                provenance: Provenance::Observed,
                cause: cause.clone(),
            },
        },
    });
    harness
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(agent, hire, true)]));
    harness.drive(Input::Tick);
    assert_eq!(
        harness.supervisor.world.agents[&agent].automatic_hold,
        Some(cause)
    );
    assert!(harness.host.spawned.is_empty());
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::Resume { .. }))
    );
}

#[test]
fn a_stale_no_host_claim_cannot_release_a_later_or_possibly_dispatched_claim() {
    fn duplicate(claim: &crate::input::HostDispatchClaim) -> crate::input::HostDispatchClaim {
        crate::input::HostDispatchClaim {
            turn: claim.turn,
            started_lsn: claim.started_lsn,
            generation: claim.generation,
        }
    }
    let mut harness = crate::testing::Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.supervisor.world.agents[&agent].open_turn.unwrap();
    let claim = harness.supervisor.prepare_host_dispatch(turn).unwrap();
    let stale = duplicate(&claim);
    harness.drive(Input::RepositoryLaunchDeferred { claim });
    assert!(!harness.supervisor.spawning.contains(&turn));
    harness.supervisor.spawning.insert(turn);
    let current = harness.supervisor.prepare_host_dispatch(turn).unwrap();
    harness.drive(Input::RepositoryLaunchDeferred { claim: stale });
    assert!(harness.supervisor.spawning.contains(&turn));
    let entered = duplicate(&current);
    assert!(harness.supervisor.enter_host_dispatch(current));
    harness.drive(Input::RepositoryLaunchDeferred { claim: entered });
    assert!(harness.supervisor.spawning.contains(&turn));
    assert!(
        !harness
            .supervisor
            .instructions
            .repository_deferred
            .contains_key(&turn)
    );
}
