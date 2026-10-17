use super::*;
use crate::effect::Effect;
use crate::input::RecordedAppend;
use crate::testing::{FakeClock, FakeRng, Harness};
use dudes_proto::record::Hex32;
use dudes_types::Timestamp;

type Machine = Supervisor<FakeClock, FakeRng>;

fn owed(paused: bool) -> (Machine, AgentId) {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["true"]);
    let mut rng = FakeRng::new();
    rng.reserve_entries(&harness.journal).unwrap();
    let mut machine = Supervisor::new(Harness::config(), FakeClock::frozen(1_787_394_700_000), rng);
    for (entry, lsn) in harness.journal.into_iter().zip(1u64..) {
        machine
            .handle(crate::Input::Replayed {
                lsn: Lsn(lsn),
                entry,
            })
            .unwrap();
    }
    if paused {
        machine
            .handle(crate::Input::Replayed {
                lsn: Lsn(9_000),
                entry: Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: Record::Pause {
                        agent,
                        browser: None,
                    },
                },
            })
            .unwrap();
    }
    (machine, agent)
}

fn command(machine: &Machine, agent: AgentId) -> RecoveryCommand {
    RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected: machine.recovery_opportunity(agent).unwrap().expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 17,
            uid: None,
        },
    }
}

fn emitted(machine: &Machine) -> (PersistToken, Entry) {
    let mut records = machine.effects().iter().filter_map(|effect| match effect {
        Effect::Persist { token, entry } => Some((*token, entry.clone())),
        _ => None,
    });
    let entry = records.next().expect("one actual emitted entry");
    assert!(records.next().is_none());
    assert!(
        machine
            .effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::Spawn { .. }))
    );
    entry
}

fn acknowledge(machine: &mut Machine, token: PersistToken, entry: Entry, lsn: Lsn) -> Handled {
    machine
        .handle(crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(lsn, (lsn, entry), None).unwrap(),
        })
        .unwrap()
}

fn acknowledge_checkpointed(
    machine: &mut Machine,
    token: PersistToken,
    entry: Entry,
    lsn: Lsn,
) -> Handled {
    let through = Lsn(lsn.0 + 1);
    let checkpoint = Entry {
        at: entry.at,
        record: Record::Checkpoint {
            projection_version: 0,
            segment: through.segment(),
            offset: through.offset(),
        },
    };
    machine
        .handle(crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(through, (lsn, entry), Some((through, checkpoint)))
                .unwrap(),
        })
        .unwrap()
}

#[test]
fn required_own_resume_and_authorization_advance_only_on_actual_acknowledgments() {
    let (mut machine, agent) = owed(true);
    let intent = command(&machine, agent);
    let work = intent.expected.work;
    let old = machine.world.turns[&work.turn].clone();
    let initial = machine.recover(intent);
    assert_eq!(
        initial.recovery().unwrap().prefix,
        RecoveryPrefix::default()
    );
    let (required_token, required) = emitted(&machine);
    assert!(matches!(required.record, Record::RecoveryRequired(_)));
    let unknown = acknowledge(
        &mut machine,
        PersistToken(999_999),
        required.clone(),
        Lsn(10_000),
    );
    assert_eq!(unknown, Handled::default());
    assert!(
        machine
            .world
            .recovery_projection()
            .unwrap()
            .0
            .current(work)
            .is_none()
    );
    let required = acknowledge_checkpointed(&mut machine, required_token, required, Lsn(10_001));
    assert_eq!(required.folded_through(), Some(Lsn(10_002)));
    assert_eq!(
        required.recovery().unwrap().prefix.required,
        Some(Lsn(10_001))
    );
    let (resume_token, resume) = emitted(&machine);
    assert!(matches!(
        resume.record,
        Record::Resume { browser: None, .. }
    ));
    let resumed = acknowledge_checkpointed(&mut machine, resume_token, resume, Lsn(10_003));
    assert_eq!(resumed.folded_through(), Some(Lsn(10_004)));
    assert_eq!(resumed.recovery().unwrap().prefix.resume, Some(Lsn(10_003)));
    let (authorized_token, authorized) = emitted(&machine);
    let Record::RecoveryAuthorized(ref payload) = authorized.record else {
        panic!("authorization follows Resume")
    };
    assert_eq!(payload.resume_lsn, Some(Lsn(10_003)));
    assert_eq!(payload.episode.origin, Lsn(10_001));
    assert!(
        matches!(&payload.plan, RecoveryPlan::RepeatIssued { issue, .. } if issue.issued.work == work && issue.lsn == old.issue_lsn)
    );
    let completed =
        acknowledge_checkpointed(&mut machine, authorized_token, authorized, Lsn(10_005));
    assert_eq!(completed.folded_through(), Some(Lsn(10_006)));
    assert_eq!(completed.recovery().unwrap().refusal, None);
    let turn = &machine.world.turns[&work.turn];
    assert_eq!(
        (&turn.task, &turn.answers, turn.issue_lsn),
        (&old.task, &old.answers, old.issue_lsn)
    );
}

#[test]
fn clean_executor_refusal_retires_only_the_exact_volatile_command() {
    let (mut machine, agent) = owed(false);
    let intent = command(&machine, agent);
    machine.recover(intent);
    let (token, entry) = emitted(&machine);
    let metadata = machine.recovery_append(token, &entry).unwrap();
    let before = machine.world.recovery_projection().unwrap().0.clone();
    let mut wrong = metadata;
    wrong.request_id = Hex32::new([99; 32]);
    assert_eq!(
        machine.recovery_refused(token, &entry, wrong, RecoveryRefusal::AuthorityUnavailable),
        Handled::default()
    );
    assert_eq!(machine.recovery_append(token, &entry), Some(metadata));
    let handled = machine.recovery_refused(
        token,
        &entry,
        metadata,
        RecoveryRefusal::AuthorityUnavailable,
    );
    assert_eq!(handled.folded_through(), None);
    assert_eq!(
        handled.recovery().unwrap().refusal,
        Some(RecoveryRefusal::AuthorityUnavailable)
    );
    assert_eq!(machine.recovery_append(token, &entry), None);
    assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
    assert!(machine.effects().is_empty());
}

#[test]
fn later_clean_refusal_retains_the_acknowledged_required_prefix() {
    let (mut machine, agent) = owed(true);
    machine.recover(command(&machine, agent));
    let (token, entry) = emitted(&machine);
    acknowledge(&mut machine, token, entry, Lsn(10_001));
    let (token, entry) = emitted(&machine);
    let metadata = machine.recovery_append(token, &entry).unwrap();
    let before = machine.world.recovery_projection().unwrap().0.clone();
    let handled = machine.recovery_refused(token, &entry, metadata, RecoveryRefusal::Unavailable);
    assert_eq!(
        handled.recovery().unwrap().prefix,
        RecoveryPrefix {
            required: Some(Lsn(10_001)),
            resume: None
        }
    );
    assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
    assert!(machine.world.agents[&agent].paused);
    assert!(machine.effects().is_empty());
}

#[test]
fn stale_predecessor_is_refused_before_a_barrier_or_rng_draw() {
    let (mut machine, agent) = owed(false);
    let mut intent = command(&machine, agent);
    intent.expected.predecessor = Some(RecoveryClaimRef {
        episode: RecoveryEpisodeId {
            work: intent.expected.work,
            origin: Lsn(1),
        },
        authorization_lsn: Lsn(2),
    });
    let drawn = machine.rng.drawn();
    let handled = machine.recover(intent);
    assert_eq!(
        handled.recovery().unwrap().refusal,
        Some(RecoveryRefusal::StalePredecessor)
    );
    assert_eq!(machine.rng.drawn(), drawn);
    assert!(machine.effects().is_empty());
}

#[test]
fn replayed_owed_work_is_held_while_live_first_issue_still_proceeds() {
    let mut live = Harness::new();
    live.launch = false;
    let (agent, _) = live.hire_with_goal(&["true"]);
    let turn = live.open_turn(agent).unwrap();
    assert!(live.supervisor.recovery_launch_allowed(agent, turn));
    let (mut replayed, agent) = owed(false);
    let turn = replayed.world.agents[&agent].open_turn.unwrap();
    assert!(!replayed.recovery_launch_allowed(agent, turn));
    replayed.handle(crate::Input::Started).unwrap();
    assert!(replayed.effects().iter().any(|effect| matches!(
        effect,
        Effect::Persist {
            entry: Entry {
                record: Record::RecoveryRequired(_),
                ..
            },
            ..
        }
    )));
    assert!(
        replayed
            .effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::Spawn { .. }))
    );
}

#[test]
fn pending_new_claim_is_exclusive_with_current_predecessor_and_an_open_question() {
    let (mut machine, agent) = owed(false);
    let work = machine.recovery_binding(agent).unwrap().work;
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_000),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::TurnEnded {
                    agent,
                    turn: work.turn,
                    mode: dudes_types::Mode::Managed,
                    provenance: dudes_types::Provenance::Observed,
                    elapsed_ms: None,
                    exit: dudes_proto::ExitStatus::UNKNOWN,
                    usage: None,
                    session_usage: None,
                    model_usage: std::collections::BTreeMap::new(),
                    last_message: None,
                    final_reply: None,
                    session: None,
                    outcome: Some(dudes_proto::TurnOutcome::NoResult),
                },
            },
        })
        .unwrap();
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_001),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::Pause {
                    agent,
                    browser: None,
                },
            },
        })
        .unwrap();
    let required = machine.recovery_opportunity(agent).unwrap().required;
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_002),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::RecoveryRequired(required),
            },
        })
        .unwrap();
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_003),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::Resume {
                    agent,
                    browser: None,
                },
            },
        })
        .unwrap();
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_004),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::HumanQuestion {
                    agent,
                    text: "Which output?".into(),
                },
            },
        })
        .unwrap();
    let mut intent = command(&machine, agent);
    intent.choice = RecoveryChoice::FreshWithRecap;
    let first_expected = intent.expected;
    machine.recover(intent);
    let (token, entry) = emitted(&machine);
    assert!(
        matches!(entry.record, Record::RecoveryAuthorized(_)),
        "separate ordinary Resume already cleared control"
    );
    acknowledge(&mut machine, token, entry, Lsn(10_001));
    let (state, _) = machine.world.recovery_projection().unwrap();
    let episode = state.current(work).unwrap();
    let claim = RecoveryClaimRef {
        episode: episode.id,
        authorization_lsn: episode.authorization_lsn.unwrap(),
    };
    assert!(matches!(
        state.claim_receipt(claim).unwrap().state,
        RecoveryClaimState::Pending
    ));
    assert_eq!(machine.world.agents[&agent].control_lsn, Some(Lsn(9_003)));
    assert_eq!(
        machine.world.agents[&agent].open_question.as_deref(),
        Some("Which output?")
    );
    let before = state.clone();
    let drawn = machine.rng.drawn();
    let mut current_expected = first_expected;
    current_expected.predecessor = Some(claim);
    let second = machine.recover(RecoveryCommand {
        request_id: Hex32::new([51; 32]),
        digest: Hex32::new([52; 32]),
        expected: current_expected,
        choice: RecoveryChoice::FreshWithRecap,
        attribution: dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 18,
            uid: Some(501),
        },
    });
    assert_eq!(
        second.recovery().unwrap().refusal,
        Some(RecoveryRefusal::Claimed)
    );
    assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
    assert_eq!(machine.rng.drawn(), drawn);
    assert!(machine.effects().is_empty());
}

#[test]
fn unseen_issue_bound_claim_refuses_stale_none_but_fresh_exact_consent_supersedes() {
    let (mut machine, agent) = owed(false);
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(8_000),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::HumanQuestion {
                    agent,
                    text: "Keep this question open".into(),
                },
            },
        })
        .unwrap();
    let required = machine.recovery_opportunity(agent).unwrap().required;
    let work = required.binding.work;
    let original = machine.world.turns[&work.turn].clone();
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_000),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::RecoveryRequired(required),
            },
        })
        .unwrap();
    let first = command(&machine, agent);
    assert!(first.expected.episode.is_some());
    assert_eq!(first.expected.predecessor, None);
    let old_expected = first.expected;
    machine.recover(first);
    let (token, entry) = emitted(&machine);
    acknowledge(&mut machine, token, entry, Lsn(10_001));
    let claim = machine
        .world
        .recovery_projection()
        .unwrap()
        .0
        .work_claim(work)
        .unwrap()
        .unwrap();
    assert!(matches!(
        machine
            .world
            .recovery_projection()
            .unwrap()
            .0
            .claim_receipt(claim)
            .unwrap()
            .state,
        RecoveryClaimState::IssueBound(_)
    ));
    let before = machine.world.recovery_projection().unwrap().0.clone();
    let drawn = machine.rng.drawn();
    let mut stale = command(&machine, agent);
    stale.request_id = Hex32::new([61; 32]);
    stale.expected = old_expected;
    let refused = machine.recover(stale);
    assert_eq!(
        refused.recovery().unwrap().refusal,
        Some(RecoveryRefusal::StalePredecessor)
    );
    assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
    assert_eq!(machine.rng.drawn(), drawn);
    assert!(machine.effects().is_empty());
    let mut fresh = command(&machine, agent);
    fresh.request_id = Hex32::new([62; 32]);
    assert_eq!(fresh.expected.predecessor, Some(claim));
    machine.recover(fresh);
    let (token, entry) = emitted(&machine);
    assert!(
        matches!(&entry.record, Record::RecoveryRequired(required) if required.predecessor == Some(claim))
    );
    acknowledge(&mut machine, token, entry, Lsn(10_003));
    let (token, entry) = emitted(&machine);
    acknowledge(&mut machine, token, entry, Lsn(10_005));
    let state = machine.world.recovery_projection().unwrap().0;
    assert!(
        matches!(state.claim_receipt(claim).unwrap().state, RecoveryClaimState::Superseded { issue, .. } if issue.lsn == original.issue_lsn && issue.issued.work == work)
    );
    let current = state.work_claim(work).unwrap().unwrap();
    assert_ne!(current, claim);
    assert_eq!(current.episode.origin, Lsn(10_003));
    assert!(
        matches!(state.claim_receipt(current).unwrap().state, RecoveryClaimState::IssueBound(issue) if issue.lsn == original.issue_lsn && issue.issued.work == work)
    );
    let turn = &machine.world.turns[&work.turn];
    assert_eq!(
        (&turn.task, &turn.answers, turn.issue_lsn),
        (&original.task, &original.answers, original.issue_lsn)
    );
}

#[test]
fn a_single_confirmed_context_failure_without_hold_keeps_ordinary_admission() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["true"]);
    let turn = harness.open_turn(agent).unwrap();
    let session = harness.supervisor.world().turns[&turn]
        .announced
        .clone()
        .expect("the fixture pins its initial session");
    let mut rng = FakeRng::new();
    rng.reserve_entries(&harness.journal).unwrap();
    let mut machine = Supervisor::new(Harness::config(), FakeClock::frozen(1_787_394_700_000), rng);
    for (entry, lsn) in harness.journal.into_iter().zip(1u64..) {
        machine
            .handle(crate::Input::Replayed {
                lsn: Lsn(lsn),
                entry,
            })
            .unwrap();
    }
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_000),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::SessionLearned {
                    agent,
                    turn,
                    session,
                },
            },
        })
        .unwrap();
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(9_001),
            entry: Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::TurnEnded {
                    agent,
                    turn,
                    mode: dudes_types::Mode::Managed,
                    provenance: dudes_types::Provenance::Observed,
                    elapsed_ms: Some(10_000),
                    exit: dudes_proto::ExitStatus::UNKNOWN,
                    usage: None,
                    session_usage: None,
                    model_usage: std::collections::BTreeMap::new(),
                    last_message: None,
                    final_reply: None,
                    session: None,
                    outcome: Some(dudes_proto::TurnOutcome::NoResult),
                },
            },
        })
        .unwrap();
    assert!(machine.world.agents[&agent].session.is_some());
    assert!(machine.world.agents[&agent].automatic_hold.is_none());
    assert_eq!(
        machine.recovery_opportunity(agent),
        Err(RecoveryRefusal::NotRecoverable)
    );
    assert!(machine.ensure_recovery_admission(agent));
    machine.decide(agent);
    assert!(machine.effects().iter().any(|effect| matches!(
        effect,
        Effect::Persist {
            entry: Entry {
                record: Record::LoopDecided { .. },
                ..
            },
            ..
        }
    )));
    assert!(machine.effects().iter().all(|effect| !matches!(
        effect,
        Effect::Persist {
            entry: Entry {
                record: Record::RecoveryRequired(_),
                ..
            },
            ..
        }
    )));
}

fn replay_machine(prefix: &[(Lsn, Entry)]) -> Machine {
    let mut rng = FakeRng::new();
    rng.reserve_entries(
        &prefix
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let adapter: Box<dyn dudes_providers::adapter::Adapter> =
        Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig::default(),
        ));
    let mut machine = Supervisor::new(Harness::config(), FakeClock::frozen(1_787_394_700_000), rng)
        .with_adapters([adapter].into_iter().collect());
    for (lsn, entry) in prefix {
        machine
            .handle(crate::Input::Replayed {
                lsn: *lsn,
                entry: entry.clone(),
            })
            .unwrap();
        assert!(
            machine.effects().is_empty(),
            "replay must not emit invalidations"
        );
    }
    machine
}

fn replay_record(machine: &mut Machine, prefix: &mut Vec<(Lsn, Entry)>, lsn: u64, record: Record) {
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    };
    machine
        .handle(crate::Input::Replayed {
            lsn: Lsn(lsn),
            entry: entry.clone(),
        })
        .unwrap();
    assert!(machine.effects().is_empty());
    prefix.push((Lsn(lsn), entry));
}

fn replay_pending(
    machine: &mut Machine,
    prefix: &mut Vec<(Lsn, Entry)>,
    agent: AgentId,
    required_lsn: u64,
    request: u8,
) -> RecoveryClaimRef {
    let view = machine.recovery_opportunity(agent).unwrap();
    let binding = view.required.binding.clone();
    replay_record(
        machine,
        prefix,
        required_lsn,
        Record::RecoveryRequired(view.required),
    );
    replay_record(
        machine,
        prefix,
        required_lsn + 1,
        Record::Resume {
            agent,
            browser: None,
        },
    );
    let episode = machine
        .world
        .recovery_projection()
        .unwrap()
        .0
        .current(binding.work)
        .unwrap()
        .id;
    let turn = TurnId::from_ulid(machine.rng.ulid());
    let session = machine.announce_session(agent, true).id().cloned();
    let authorization = RecoveryAuthorized {
        episode,
        binding,
        request_id: Hex32::new([request; 32]),
        request_digest: Hex32::new([request + 1; 32]),
        attribution: dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 17,
            uid: None,
        },
        expected_control: view.expected.control_lsn,
        resulting_control: Some(Lsn(required_lsn + 1)),
        resume_lsn: Some(Lsn(required_lsn + 1)),
        plan: RecoveryPlan::New {
            turn,
            context: RecoveryContext::Fresh {
                session,
                recap: RecoveryRecap::default(),
            },
        },
    };
    assert_eq!(
        machine
            .world
            .prepare_recovery(RecoveryTransition::Authorized(&authorization)),
        Ok(())
    );
    replay_record(
        machine,
        prefix,
        required_lsn + 2,
        Record::RecoveryAuthorized(Box::new(authorization)),
    );
    RecoveryClaimRef {
        episode,
        authorization_lsn: Lsn(required_lsn + 2),
    }
}

fn pending_history() -> (Vec<(Lsn, Entry)>, AgentId, RecoveryClaimRef) {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["false"]);
    harness.drive(crate::Input::ChildExited {
        final_reply: None,
        elapsed_ms: Some(10_000),
        turn: harness.open_turn(agent).unwrap(),
        exit: dudes_proto::ExitStatus {
            code: Some(1),
            signal: None,
        },
        usage: None,
        model_usage: std::collections::BTreeMap::new(),
        outcome: dudes_proto::TurnOutcome::NoResult,
        last_message: None,
    });
    harness.drive(crate::Input::Command(crate::Command::Pause {
        agent,
        browser: None,
    }));
    let mut prefix = harness
        .journal
        .into_iter()
        .zip(1u64..)
        .map(|(entry, lsn)| (Lsn(lsn), entry))
        .collect::<Vec<_>>();
    let mut machine = replay_machine(&prefix);
    let claim = replay_pending(&mut machine, &mut prefix, agent, 9_000, 91);
    (prefix, agent, claim)
}

fn drain_invalidations(
    machine: &mut Machine,
    mut lsn: u64,
) -> Vec<dudes_proto::recovery::RecoveryInvalidated> {
    machine.handle(crate::Input::Started).unwrap();
    let mut invalidated = Vec::new();
    for _ in 0..16 {
        let emitted = machine
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Persist {
                    token,
                    entry:
                        Entry {
                            record: Record::RecoveryInvalidated(record),
                            at,
                        },
                } => Some((
                    token,
                    Entry {
                        at,
                        record: Record::RecoveryInvalidated(record.clone()),
                    },
                    record,
                )),
                _ => None,
            });
        if let Some((token, entry, record)) = emitted {
            acknowledge(machine, token, entry, Lsn(lsn));
            invalidated.push(record);
            lsn += 1;
        } else if !machine.recovery_discovery.busy() {
            break;
        } else {
            machine.handle(crate::Input::Tick).unwrap();
        }
    }
    assert!(!machine.recovery_discovery.busy());
    invalidated
}

#[test]
fn replay_retains_the_earlier_different_goal_witness_after_a_same_goal_set() {
    let (mut prefix, agent, claim) = pending_history();
    let mut machine = replay_machine(&prefix);
    let old_goal = machine.world.goals[&claim.episode.work.goal].goal.clone();
    let mut other_goal = old_goal.clone();
    other_goal.id = dudes_types::GoalId::from_parts(98, 11);
    replay_record(
        &mut machine,
        &mut prefix,
        10_000,
        Record::GoalSet {
            agent,
            goal: other_goal,
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        10_001,
        Record::GoalSet {
            agent,
            goal: old_goal,
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    let mut replayed = replay_machine(&prefix);
    assert!(matches!(
        replayed
            .world
            .recovery_projection()
            .unwrap()
            .0
            .claim_receipt(claim)
            .unwrap()
            .state,
        RecoveryClaimState::Pending
    ));
    let invalidated = drain_invalidations(&mut replayed, 30_000);
    assert_eq!(
        invalidated,
        vec![dudes_proto::recovery::RecoveryInvalidated {
            claim,
            evidence: dudes_proto::recovery::RecoveryInvalidationEvidence::RecordedChange {
                reason: dudes_proto::recovery::RecoveryInvalidationReason::GoalChanged,
                lsn: Lsn(10_000)
            }
        }]
    );
    assert!(matches!(
        replayed
            .world
            .recovery_projection()
            .unwrap()
            .0
            .claim_receipt(claim)
            .unwrap()
            .state,
        RecoveryClaimState::Invalidated { .. }
    ));
}

#[test]
fn crash_replay_discovers_multiple_stale_pending_claims_including_an_overwritten_last_turn() {
    let (mut prefix, agent, first) = pending_history();
    let mut machine = replay_machine(&prefix);
    let mut goal = machine.world.goals[&first.episode.work.goal].goal.clone();
    goal.id = dudes_types::GoalId::from_parts(98, 12);
    let goal_id = goal.id;
    let selection = machine.world.turns[&first.episode.work.turn]
        .model_selection
        .clone();
    let turn = TurnId::from_parts(98, 13);
    replay_record(
        &mut machine,
        &mut prefix,
        20_000,
        Record::GoalSet {
            agent,
            goal: goal.clone(),
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        20_001,
        Record::LoopDecided {
            agent,
            input_digest: String::new(),
            decision: dudes_proto::LoopDecision::Continue {
                task: "original B".into(),
            },
            gate_trace: Vec::new(),
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        20_002,
        Record::TurnIssued {
            agent,
            turn,
            goal: goal_id,
            task: "original B".into(),
            session: None,
            mode: dudes_types::Mode::Managed,
            model_selection: selection.clone(),
            lsn_of_decision: Lsn(20_001),
            answers: Vec::new(),
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        20_003,
        Record::TurnEnded {
            agent,
            turn,
            mode: dudes_types::Mode::Managed,
            provenance: dudes_types::Provenance::Observed,
            elapsed_ms: Some(10_000),
            exit: dudes_proto::ExitStatus {
                code: Some(1),
                signal: None,
            },
            usage: None,
            session_usage: None,
            model_usage: std::collections::BTreeMap::new(),
            last_message: None,
            final_reply: None,
            session: None,
            outcome: Some(dudes_proto::TurnOutcome::NoResult),
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        20_004,
        Record::Pause {
            agent,
            browser: None,
        },
    );
    let second = replay_pending(&mut machine, &mut prefix, agent, 20_005, 93);
    let hidden_by = TurnId::from_parts(98, 14);
    replay_record(
        &mut machine,
        &mut prefix,
        20_008,
        Record::LoopDecided {
            agent,
            input_digest: String::new(),
            decision: dudes_proto::LoopDecision::Continue {
                task: "still owed B".into(),
            },
            gate_trace: Vec::new(),
        },
    );
    replay_record(
        &mut machine,
        &mut prefix,
        20_009,
        Record::TurnIssued {
            agent,
            turn: hidden_by,
            goal: goal_id,
            task: "still owed B".into(),
            session: None,
            mode: dudes_types::Mode::Managed,
            model_selection: selection,
            lsn_of_decision: Lsn(20_008),
            answers: Vec::new(),
        },
    );
    assert_ne!(
        machine.world.goals[&goal_id].last_turn,
        Some(second.episode.work.turn)
    );
    goal.id = dudes_types::GoalId::from_parts(98, 15);
    replay_record(
        &mut machine,
        &mut prefix,
        21_000,
        Record::GoalSet {
            agent,
            goal,
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    let mut recovered = replay_machine(&prefix);
    let before = recovered.world.turns[&hidden_by].clone();
    let hops = recovered.world.goals[&goal_id].turns_on_goal;
    for claim in [first, second] {
        assert!(matches!(
            recovered
                .world
                .recovery_projection()
                .unwrap()
                .0
                .claim_receipt(claim)
                .unwrap()
                .state,
            RecoveryClaimState::Pending
        ));
    }
    let records = drain_invalidations(&mut recovered, 30_000);
    assert_eq!(records.len(), 2);
    for claim in [first, second] {
        assert!(records.iter().any(|record| record.claim == claim));
        assert!(matches!(
            recovered
                .world
                .recovery_projection()
                .unwrap()
                .0
                .claim_receipt(claim)
                .unwrap()
                .state,
            RecoveryClaimState::Invalidated { .. }
        ));
    }
    let after = &recovered.world.turns[&hidden_by];
    assert_eq!(
        (&after.task, &after.answers, after.issue_lsn, after.status),
        (
            &before.task,
            &before.answers,
            before.issue_lsn,
            before.status
        )
    );
    assert_eq!(recovered.world.goals[&goal_id].turns_on_goal, hops);
}

#[test]
fn whole_utf8_recap_fits_only_after_all_mandatory_task_and_answers() {
    let recap = "é".repeat(4_096);
    assert_eq!(recap.len(), 8_192);
    let separator = "\n\nPrevious work context:\n";
    let answer = "Current answer: keep the green output exactly.\n";
    let padding = dudes_policy::r#loop::NEXT_WORK_TASK_MAX_BYTES
        - separator.len()
        - recap.len()
        - answer.len();
    let mandatory = format!("{answer}{}", "m".repeat(padding));
    let exact = with_recap(&mandatory, &recap);
    assert_eq!(exact, format!("{mandatory}{separator}{recap}"));
    assert_eq!(exact.len(), 65_536);
    assert!(exact.starts_with(answer));
    let over = format!("{mandatory}m");
    assert_eq!(with_recap(&over, &recap), over);
    let full = format!("{answer}{}", "m".repeat(65_536 - answer.len()));
    assert_eq!(with_recap(&full, &recap), full);
}

#[test]
fn generated_recap_is_bounded_utf8_from_the_exact_recorded_goal() {
    let mut harness = Harness::new();
    harness.launch = false;
    let agent = harness.hire();
    harness.drive(crate::Input::Command(crate::Command::GoalSet {
        completion_check: None,
        agent,
        text: "é".repeat(8_192),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["false".into()],
            timeout_s: 10,
        },
        budget: dudes_proto::GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let binding = harness.supervisor.recovery_binding(agent).unwrap();
    let recap = harness.supervisor.recovery_recap(&binding);
    assert!((8_189..=8_192).contains(&recap.text.len()));
    assert!(recap.text.starts_with(
        "Prior execution effects are unknown. This recap does not restore conversation history."
    ));
    assert!(recap.text.contains("Recorded goal: é"));
    assert_eq!(
        recap.sources,
        vec![harness.supervisor.world.goals[&binding.work.goal].installed_lsn]
    );
    assert!(recap.delivered_answers.is_empty());
}
