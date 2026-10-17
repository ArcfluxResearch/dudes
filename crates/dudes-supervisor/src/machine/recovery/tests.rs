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
