use super::*;
use crate::{
    RecoveryAvailability, RecoverySubjectKey, RecoverySubjectUse, with_recovery_operation,
};

fn browser_prefix(pending: bool) -> (Machine, AgentId, RecoverySubjectKey, Vec<(Lsn, Entry)>) {
    let (mut prefix, agent) = if pending {
        failure_source(None)
    } else {
        let mut harness = Harness::new();
        harness.launch = false;
        let (agent, _) = harness.hire_with_goal(&["false"]);
        (
            harness
                .journal
                .into_iter()
                .zip(1u64..)
                .map(|(entry, at)| (Lsn(at), entry))
                .collect(),
            agent,
        )
    };
    let mut machine = replay_machine(&prefix);
    let claim = historical_browser_claim(&mut machine, &mut prefix, agent);
    let (state, _) = machine.world.recovery_projection().unwrap();
    let receipt = state.claim_receipt(claim).unwrap();
    let purpose = match receipt.state {
        RecoveryClaimState::Pending => RecoverySubjectUse::AdmitNew,
        RecoveryClaimState::IssueBound(issue) => RecoverySubjectUse::DispatchIssued(issue.clone()),
        _ => panic!("expected an active browser claim"),
    };
    let key = RecoverySubjectKey::new(claim, receipt.authorization.clone(), purpose);
    (machine, agent, key, prefix)
}

#[test]
fn live_pending_operation_admits_the_exact_reserved_recovery_context() {
    let (machine, agent, key) = browser(true);
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(
                machine.recovery_dispatch_attribution_allowed(key.claim().episode.work, operation)
            );
            assert!(machine.recovery_admission_allowed(agent, operation));
            let context = machine
                .recovery_new_context(agent, operation)
                .unwrap()
                .unwrap();
            let RecoveryPlan::New { turn, .. } = key.authorization().plan else {
                unreachable!()
            };
            assert_eq!(context.turn, turn);
        },
    );
}

#[test]
fn live_issue_bound_operation_admits_only_its_exact_durable_issue() {
    let (mut machine, agent, key) = browser(false);
    let RecoverySubjectUse::DispatchIssued(issue) = key.purpose() else {
        unreachable!()
    };
    machine.live_issues.insert(issue.issued.work.turn);
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(machine.recovery_dispatch_attribution_allowed(issue.issued.work, operation));
            assert!(machine.recovery_admission_allowed(agent, operation));
            assert!(machine.recovery_launch_allowed(agent, issue.issued.work.turn, operation));
        },
    );
}

#[test]
fn unavailable_or_uncertain_issue_bound_reissue_holds_the_original_claim() {
    for status in 0..3 {
        let (mut machine, agent, key) = browser(false);
        let before = machine.world.recovery_projection().unwrap().0.clone();
        let RecoverySubjectUse::DispatchIssued(issue) = key.purpose() else {
            unreachable!()
        };
        machine.live_issues.insert(issue.issued.work.turn);
        let row = machine.world.turns[&issue.issued.work.turn].clone();
        let keys = if status == 0 {
            &[][..]
        } else {
            std::slice::from_ref(&key)
        };
        with_recovery_operation(
            keys,
            |_| {
                if status == 1 {
                    RecoveryAvailability::EstablishedUnavailable
                } else {
                    RecoveryAvailability::Indeterminate
                }
            },
            |operation| {
                machine.reissue_turn(
                    issue.issued.work.turn,
                    agent,
                    &row.task,
                    row.model_selection,
                    operation,
                );
            },
        );
        assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
        assert!(
            !machine.effects().iter().any(|effect| matches!(
                effect,
                Effect::Persist {
                    entry: Entry {
                        record: Record::RecoveryRequired(_)
                            | Record::RecoveryInvalidated(_)
                            | Record::TurnIssued { .. },
                        ..
                    },
                    ..
                } | Effect::Spawn { .. }
            )),
            "held issue must keep its claim and task: {:?}",
            machine.effects()
        );
    }
}

fn browser(pending: bool) -> (Machine, AgentId, RecoverySubjectKey) {
    let (machine, agent, key, _) = browser_prefix(pending);
    (machine, agent, key)
}

fn keys_at(world: &crate::World) -> Vec<RecoverySubjectKey> {
    let (state, _) = world.recovery_projection().unwrap();
    let mut keys = Vec::new();
    for (&turn, row) in &world.turns {
        let Some(goal) = row.goal else { continue };
        let work = RecoveryWork {
            agent: row.agent,
            goal,
            turn,
        };
        let Ok(Some(claim)) = state.work_claim(work) else {
            continue;
        };
        let receipt = state.claim_receipt(claim).unwrap();
        let purpose = match receipt.state {
            RecoveryClaimState::Pending => RecoverySubjectUse::AdmitNew,
            RecoveryClaimState::IssueBound(issue) => {
                RecoverySubjectUse::DispatchIssued(issue.clone())
            }
            _ => continue,
        };
        let key = RecoverySubjectKey::new(claim, receipt.authorization.clone(), purpose);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

fn scoped_handle(
    machine: &mut Machine,
    input: crate::Input,
    observe: impl FnMut(&RecoverySubjectKey) -> RecoveryAvailability,
) -> Handled {
    let mut projected = machine.world.clone();
    if let crate::Input::Persisted { append, .. } = &input {
        projected
            .apply_checked(append.lsn(), append.entry())
            .unwrap_or_else(|error| {
                panic!(
                    "projected {:?} at {:?} after {:?}: {error:?}",
                    append.entry().record.kind(),
                    append.lsn(),
                    machine.world.last_applied_lsn
                )
            });
    }
    let keys = keys_at(&projected);
    with_recovery_operation(&keys, observe, |operation| {
        machine.handle(input, operation).unwrap()
    })
}

#[derive(Default)]
struct Driven {
    records: Vec<(Lsn, Entry)>,
    spawned: Vec<TurnId>,
    purposes: Vec<RecoverySubjectUse>,
    invocations: usize,
}

fn drive_live(machine: &mut Machine) -> Driven {
    drive_until_hold(machine, None)
}

fn parked_new_issue(machine: &mut Machine) -> (PersistToken, Entry) {
    let mut effects = std::collections::VecDeque::from(machine.take_effects());
    let mut next = machine.world.last_applied_lsn.unwrap().0 + 1;
    for _ in 0..128 {
        let effect = effects
            .pop_front()
            .expect("the actual producer must reach its reserved issue");
        let input = match effect {
            Effect::Persist { token, entry } => {
                if matches!(entry.record, Record::TurnIssued { .. }) {
                    assert!(effects.is_empty());
                    return (token, entry);
                }
                let lsn = Lsn(next);
                next += 1;
                crate::Input::Persisted {
                    token,
                    append: RecordedAppend::try_new(lsn, (lsn, entry), None).unwrap(),
                }
            }
            Effect::SampleInstructions {
                request,
                goal,
                baseline,
                ..
            } => Harness::mock_instruction_sample(machine.world(), request, goal, baseline),
            Effect::Spawn { .. } => panic!("the issue must not be acknowledged or spawned"),
            _ => continue,
        };
        scoped_handle(machine, input, |_| RecoveryAvailability::Live);
        effects.extend(machine.take_effects());
    }
    panic!("the actual producer must settle within the fixture bound")
}

#[test]
fn unwritten_issue_completion_requires_the_exact_parked_entry_and_claim() {
    let (mut machine, agent, key, _) = authorize_live(true);
    let (token, entry) = parked_new_issue(&mut machine);
    let metadata = machine
        .recovery_issue_append(token, &entry)
        .unwrap()
        .unwrap();
    assert_eq!(metadata.subject(), &key);
    let RecoveryPlan::New { turn, .. } = key.authorization().plan else {
        unreachable!()
    };
    assert_eq!(metadata.turn(), turn);
    assert!(matches!(machine.ledger[&token].then, Then::Spawn { .. }));
    assert!(!machine.world.turns.contains_key(&turn));
    let world = machine.world.clone();
    let spend = machine.spend.clone();
    let drawn = machine.rng.drawn();
    let ledger = machine.ledger.keys().copied().collect::<Vec<_>>();
    let unknown = PersistToken(u64::MAX);
    assert_eq!(
        machine.recovery_issue_append(unknown, &entry),
        Err(RecoveryRefusal::Unavailable)
    );
    assert!(!machine.recovery_issue_not_written(unknown, &entry, &metadata));
    let mut changed = entry.clone();
    let Record::TurnIssued { task, .. } = &mut changed.record else {
        unreachable!()
    };
    task.push_str(" changed");
    assert_eq!(
        machine.recovery_issue_append(token, &changed),
        Err(RecoveryRefusal::Unavailable)
    );
    assert!(!machine.recovery_issue_not_written(token, &changed, &metadata));
    let mut wrong = metadata.clone();
    wrong.turn = key.claim().episode.work.turn;
    assert!(!machine.recovery_issue_not_written(token, &entry, &wrong));
    let mut wrong = metadata.clone();
    let mut authorization = key.authorization().clone();
    authorization.request_digest = Hex32::new([171; 32]);
    wrong.subject =
        RecoverySubjectKey::new(key.claim(), authorization, RecoverySubjectUse::AdmitNew);
    assert!(!machine.recovery_issue_not_written(token, &entry, &wrong));
    assert_eq!(machine.ledger.keys().copied().collect::<Vec<_>>(), ledger);
    assert_eq!(
        machine.recovery_issue_append(token, &entry),
        Ok(Some(metadata.clone()))
    );
    assert!(machine.recovery_issue_not_written(token, &entry, &metadata));
    assert_eq!(
        machine.recovery_issue_append(token, &entry),
        Err(RecoveryRefusal::Unavailable)
    );
    assert!(!machine.recovery_issue_not_written(token, &entry, &metadata));
    assert_eq!(
        machine.world.recovery_projection().unwrap().0,
        world.recovery_projection().unwrap().0
    );
    assert_eq!(machine.world.instructions, world.instructions);
    let current = &machine.world.turns[&key.claim().episode.work.turn];
    let original = &world.turns[&key.claim().episode.work.turn];
    assert_eq!(
        (
            &current.task,
            &current.answers,
            current.issue_lsn,
            current.status
        ),
        (
            &original.task,
            &original.answers,
            original.issue_lsn,
            original.status
        )
    );
    assert_eq!(
        machine.world.goals[&key.claim().episode.work.goal].turns_on_goal,
        world.goals[&key.claim().episode.work.goal].turns_on_goal
    );
    assert_eq!(machine.spend, spend);
    assert_eq!(machine.rng.drawn(), drawn);
    assert!(machine.effects().is_empty());
    assert!(!machine.recovery_admission_allowed(agent, &crate::RecoveryOperation::empty()));
}

fn drive_until_hold(machine: &mut Machine, denied: Option<dudes_proto::RecordKind>) -> Driven {
    let mut trace = Driven::default();
    let mut effects = std::collections::VecDeque::from(machine.take_effects());
    let mut next = machine.world.last_applied_lsn.unwrap().0 + 1;
    while let Some(effect) = effects.pop_front() {
        assert!(trace.invocations < 128, "the scoped fixture must settle");
        let input = match effect {
            Effect::Persist { token, entry } => {
                let lsn = Lsn(next);
                next += 1;
                trace.records.push((lsn, entry.clone()));
                crate::Input::Persisted {
                    token,
                    append: RecordedAppend::try_new(lsn, (lsn, entry), None).unwrap(),
                }
            }
            Effect::SampleInstructions {
                request,
                goal,
                baseline,
                ..
            } => Harness::mock_instruction_sample(machine.world(), request, goal, baseline),
            Effect::Spawn { turn, .. } => {
                trace.spawned.push(turn);
                continue;
            }
            _ => continue,
        };
        let held = matches!(&input, crate::Input::Persisted { append, .. } if Some(append.entry().record.kind()) == denied);
        trace.invocations += 1;
        scoped_handle(machine, input, |key| {
            trace.purposes.push(key.purpose().clone());
            if held {
                RecoveryAvailability::Indeterminate
            } else {
                RecoveryAvailability::Live
            }
        });
        if held {
            return trace;
        }
        effects.extend(machine.take_effects());
    }
    trace
}

#[test]
fn inherited_issue_bound_reissue_requires_a_new_episode_even_with_an_old_live_key() {
    for unavailable in [false, true] {
        let (mut machine, agent, key) = browser(false);
        let RecoverySubjectUse::DispatchIssued(issue) = key.purpose() else {
            unreachable!()
        };
        assert!(!machine.live_issues.contains(&issue.issued.work.turn));
        let row = machine.world.turns[&issue.issued.work.turn].clone();
        with_recovery_operation(
            std::slice::from_ref(&key),
            |_| {
                if unavailable {
                    RecoveryAvailability::EstablishedUnavailable
                } else {
                    RecoveryAvailability::Live
                }
            },
            |operation| {
                assert!(!machine.recovery_launch_allowed(agent, issue.issued.work.turn, operation));
                machine.reissue_turn(
                    issue.issued.work.turn,
                    agent,
                    &row.task,
                    row.model_selection,
                    operation,
                );
            },
        );
        let (_, entry) = emitted(&machine);
        assert!(
            matches!(entry.record, Record::RecoveryRequired(ref required)
            if required.predecessor == Some(key.claim()) && required.reason == RecoveryReason::MissingSpawn)
        );
    }
}

#[test]
fn live_pending_and_repeat_paths_refresh_each_acknowledgment_scope() {
    for pending in [false, true] {
        let (mut machine, agent, key, _) = authorize_live(pending);
        let driven = drive_live(&mut machine);
        assert_eq!(
            driven.spawned.len(),
            1,
            "pending={pending}, records={:?}",
            driven
                .records
                .iter()
                .map(|(_, entry)| entry.record.kind())
                .collect::<Vec<_>>()
        );
        if pending {
            let RecoveryPlan::New { turn, .. } = key.authorization().plan else {
                unreachable!()
            };
            assert_eq!(driven.spawned, vec![turn]);
            assert!(
                driven
                    .purposes
                    .iter()
                    .any(|purpose| matches!(purpose, RecoverySubjectUse::AdmitNew))
            );
            assert!(
                driven
                    .purposes
                    .iter()
                    .any(|purpose| matches!(purpose, RecoverySubjectUse::DispatchIssued(_)))
            );
        } else {
            assert_eq!(driven.spawned, vec![key.claim().episode.work.turn]);
        }
        assert!(driven.invocations > 0);
        assert!(machine.world.agents.contains_key(&agent));
    }
}

fn authorize_live(pending: bool) -> (Machine, AgentId, RecoverySubjectKey, Vec<(Lsn, Entry)>) {
    authorize_kind(if pending {
        AuthorizationKind::Fresh
    } else {
        AuthorizationKind::Repeat
    })
}

#[derive(Clone, Copy)]
enum AuthorizationKind {
    Fresh,
    TryRecorded,
    Repeat,
    Mcp,
}

fn recorded_context_key(
    machine: &Machine,
    key: &RecoverySubjectKey,
    authorization: &mut Entry,
) -> RecoverySubjectKey {
    let session = machine
        .world
        .recovery_facts(&key.authorization().binding)
        .unwrap()
        .confirmed_session
        .unwrap();
    let Record::RecoveryAuthorized(payload) = &mut authorization.record else {
        unreachable!()
    };
    let RecoveryPlan::New { context, .. } = &mut payload.plan else {
        unreachable!()
    };
    *context = RecoveryContext::TryRecorded { session };
    RecoverySubjectKey::new(
        key.claim(),
        (**payload).clone(),
        RecoverySubjectUse::AdmitNew,
    )
}

fn authorize_kind(
    kind: AuthorizationKind,
) -> (Machine, AgentId, RecoverySubjectKey, Vec<(Lsn, Entry)>) {
    let pending = !matches!(kind, AuthorizationKind::Repeat);
    let recorded = matches!(kind, AuthorizationKind::TryRecorded);
    let (_, agent, mut original_key, mut prefix) = browser_prefix(pending);
    let (original_position, mut authorization) = prefix.pop().unwrap();
    assert_eq!(original_position, original_key.claim().authorization_lsn);
    let mut machine = current_named_adapter(replay_machine(&prefix));
    if recorded {
        original_key = recorded_context_key(&machine, &original_key, &mut authorization);
    }
    if !pending {
        scoped_handle(&mut machine, crate::Input::Started, |_| {
            panic!("the unclaimed prefix has no browser consent")
        });
        let cleanup = drive_live(&mut machine);
        assert!(cleanup.spawned.is_empty());
        prefix.extend(cleanup.records);
    }
    if matches!(kind, AuthorizationKind::Mcp) {
        let at = machine.world.last_applied_lsn.unwrap().0 + 1;
        replay_record(
            &mut machine,
            &mut prefix,
            at,
            Record::AgentMcpServersChanged {
                agent,
                mcp_servers: Vec::new(),
            },
        );
    }
    let position = Lsn(machine.world.last_applied_lsn.unwrap().0 + 1);
    let key = RecoverySubjectKey::new(
        RecoveryClaimRef {
            authorization_lsn: position,
            ..original_key.claim()
        },
        original_key.authorization().clone(),
        original_key.purpose().clone(),
    );
    let (required_at, required) = prefix
        .iter()
        .rev()
        .find_map(|(at, entry)| match &entry.record {
            Record::RecoveryRequired(required)
                if required.binding.work == key.claim().episode.work =>
            {
                Some((*at, required.clone()))
            }
            _ => None,
        })
        .unwrap();
    let mut intent = command(&machine, agent);
    intent.request_id = key.authorization().request_id;
    intent.digest = key.authorization().request_digest;
    intent.attribution = key.authorization().attribution;
    intent.expected.control_lsn = key.authorization().expected_control;
    intent.choice = if recorded {
        RecoveryChoice::TryRecorded
    } else if pending {
        RecoveryChoice::FreshWithRecap
    } else {
        RecoveryChoice::RepeatIssuedFresh
    };
    machine
        .rng
        .reserve_entries(std::slice::from_ref(&authorization))
        .unwrap();
    machine.persist(
        authorization.record,
        Then::Recovery(RecoveryThen::Authorized(Box::new(Continuing {
            command: intent,
            required,
            episode: Some(key.claim().episode),
            prefix: RecoveryPrefix {
                required: Some(required_at),
                resume: key.authorization().resume_lsn,
            },
        }))),
    );
    let (token, entry) = emitted(&machine);
    prefix.push((position, entry.clone()));
    let handled = scoped_handle(
        &mut machine,
        crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(position, (position, entry), None).unwrap(),
        },
        |_| RecoveryAvailability::Live,
    );
    assert_eq!(handled.folded_through(), Some(position));
    (machine, agent, key, prefix)
}

fn changed_key(
    key: &RecoverySubjectKey,
    field: &str,
    source: &dudes_proto::recovery::RecoveryIssue,
) -> RecoverySubjectKey {
    let mut claim = key.claim();
    let mut authorization = key.authorization().clone();
    let mut purpose = key.purpose().clone();
    match field {
        "claim" => claim.authorization_lsn = Lsn(claim.authorization_lsn.0 + 1),
        "episode" => claim.episode.origin = Lsn(claim.episode.origin.0 + 1),
        "request" => authorization.request_id = Hex32::new([91; 32]),
        "digest" => authorization.request_digest = Hex32::new([92; 32]),
        "hire" => authorization.binding.hired_lsn = Lsn(93),
        "provider" => authorization.binding.provider = dudes_types::Provider::Codex,
        "model" => authorization.binding.model_selection = None,
        "lease" => {
            authorization.binding.worktree = Some(dudes_types::WorktreeId::from_bytes([94; 16]));
        }
        "ticket" => {
            authorization.binding.ticket = Some(dudes_proto::diff_stamp::TicketBinding {
                card: dudes_types::CardId::from_parts(95, 1),
                round: 1,
                goal: authorization.binding.work.goal,
                claimant: authorization.binding.work.agent,
            });
        }
        "work" => authorization.binding.work.turn = TurnId::from_parts(96, 1),
        "plan" => match &mut authorization.plan {
            RecoveryPlan::New { turn, .. } => *turn = TurnId::from_parts(97, 1),
            RecoveryPlan::RepeatIssued { issue, .. } => issue.lsn = Lsn(issue.lsn.0 + 1),
        },
        "expected" => authorization.expected_control = Some(Lsn(98)),
        "control" => authorization.resulting_control = None,
        "resume" => authorization.resume_lsn = None,
        "browser" => {
            authorization.attribution = dudes_proto::recovery::RecoveryAttribution::BrowserSession {
                session_digest: Hex32::new([99; 32]),
            }
        }
        "purpose" => {
            purpose = match purpose {
                RecoverySubjectUse::AdmitNew => RecoverySubjectUse::DispatchIssued(source.clone()),
                RecoverySubjectUse::DispatchIssued(_) => RecoverySubjectUse::AdmitNew,
            }
        }
        "issue" => {
            let mut issue = source.clone();
            issue.lsn = Lsn(issue.lsn.0 + 1);
            purpose = RecoverySubjectUse::DispatchIssued(issue);
        }
        _ => unreachable!(),
    }
    let changed = RecoverySubjectKey::new(claim, authorization, purpose);
    assert_ne!(changed, *key);
    changed
}

#[test]
fn mismatched_subject_fields_never_admit_or_establish_authority_loss() {
    for pending in [false, true] {
        let (mut machine, agent, key) = browser(pending);
        if let RecoverySubjectUse::DispatchIssued(issue) = key.purpose() {
            machine.live_issues.insert(issue.issued.work.turn);
        }
        let source = machine
            .world
            .recovery_facts(&key.authorization().binding)
            .unwrap()
            .source_issue;
        let before = machine.world.recovery_projection().unwrap().0.clone();
        for field in [
            "claim", "episode", "request", "digest", "hire", "provider", "model", "lease",
            "ticket", "work", "plan", "expected", "control", "resume", "browser", "purpose",
            "issue",
        ] {
            let wrong = changed_key(&key, field, &source);
            for unavailable in [false, true] {
                with_recovery_operation(
                    std::slice::from_ref(&wrong),
                    |_| {
                        if unavailable {
                            RecoveryAvailability::EstablishedUnavailable
                        } else {
                            RecoveryAvailability::Live
                        }
                    },
                    |operation| {
                        assert_eq!(
                            machine
                                .recovery_browser_availability(key.claim().episode.work, operation),
                            RecoveryAvailability::Indeterminate,
                            "{field}"
                        );
                        assert!(
                            !machine.recovery_admission_allowed(agent, operation),
                            "{field}"
                        );
                        machine.handle(crate::Input::Tick, operation).unwrap();
                    },
                );
                assert!(
                    !machine.effects().iter().any(|effect| matches!(
                        effect,
                        Effect::Spawn { .. }
                            | Effect::Persist {
                                entry: Entry {
                                    record: Record::RecoveryInvalidated(_)
                                        | Record::TurnIssued { .. },
                                    ..
                                },
                                ..
                            }
                    )),
                    "{field}"
                );
                assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
            }
        }
    }
}

#[test]
fn a_store_ahead_key_joins_after_the_actual_authorization_fold() {
    let (_, agent, key, mut prefix) = browser_prefix(true);
    let (at, authorized) = prefix.pop().unwrap();
    let mut machine = replay_machine(&prefix);
    machine.persist(authorized.record, Then::Decide { agent });
    let (token, entry) = emitted(&machine);
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert_eq!(
                machine.recovery_browser_availability(key.claim().episode.work, operation),
                RecoveryAvailability::Indeterminate
            );
            let handled = machine
                .handle(
                    crate::Input::Persisted {
                        token,
                        append: RecordedAppend::try_new(at, (at, entry), None).unwrap(),
                    },
                    operation,
                )
                .unwrap();
            assert_eq!(handled.folded_through(), Some(at));
            assert!(machine.recovery_admission_allowed(agent, operation));
            assert!(machine.effects().iter().any(|effect| matches!(effect,
            Effect::Persist { entry: Entry { record: Record::LoopDecided { agent: owner, decision: dudes_proto::LoopDecision::Continue { .. }, .. }, .. }, .. } if *owner == agent)));
        },
    );
    machine
        .handle(
            crate::Input::Replayed {
                lsn: Lsn(at.0 + 100),
                entry: Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: Record::AgentMcpServersChanged {
                        agent,
                        mcp_servers: Vec::new(),
                    },
                },
            },
            &crate::RecoveryOperation::empty(),
        )
        .unwrap();
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(machine.recovery_admission_allowed(agent, operation));
        },
    );
}

#[test]
fn planned_issue_bound_and_invalidated_claims_cannot_prepare_an_ordinary_new_turn() {
    let (mut machine, agent, _, _) = authorize_live(true);
    let driven = drive_live(&mut machine);
    let turn = driven.spawned[0];
    let work = machine.world.turns[&turn]
        .goal
        .map(|goal| RecoveryWork { agent, goal, turn })
        .unwrap();
    assert!(
        machine
            .world
            .recovery_projection()
            .unwrap()
            .0
            .current(work)
            .is_none()
    );
    let keys = keys_at(machine.world());
    let row = machine.world.agents[&agent].clone();
    with_recovery_operation(
        &keys,
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(machine.recovery_new_context(agent, operation).is_err());
            assert!(
                machine
                    .prepare_new_issue(agent, &row, "must retain the issued task", operation)
                    .is_none()
            );
        },
    );
    assert!(machine.effects().is_empty());
    let (mut pending, agent, key) = browser(true);
    scoped_handle(&mut pending, crate::Input::Tick, |_| {
        RecoveryAvailability::EstablishedUnavailable
    });
    let (token, entry) = emitted(&pending);
    let at = Lsn(10_000);
    scoped_handle(
        &mut pending,
        crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(at, (at, entry), None).unwrap(),
        },
        |_| RecoveryAvailability::Live,
    );
    let RecoveryPlan::New { turn, .. } = key.authorization().plan else {
        unreachable!()
    };
    assert!(
        pending
            .world
            .recovery_projection()
            .unwrap()
            .0
            .current(RecoveryWork {
                turn,
                ..key.claim().episode.work
            })
            .is_none()
    );
    let row = pending.world.agents[&agent].clone();
    assert!(
        pending
            .recovery_new_context(agent, &crate::RecoveryOperation::empty())
            .is_err()
    );
    assert!(
        pending
            .prepare_new_issue(
                agent,
                &row,
                "cannot use invalid consent",
                &crate::RecoveryOperation::empty()
            )
            .is_none()
    );
}

#[test]
fn spawned_result_is_recorded_after_authority_loss_and_consumed_context_is_ordinary() {
    let (mut machine, agent, key, _) = authorize_live(true);
    let driven = drive_live(&mut machine);
    let turn = driven.spawned[0];
    machine
        .handle(
            crate::Input::Spawned {
                turn,
                key: crate::AdoptionKey {
                    pid: 4242,
                    start_time: 10,
                    boot: None,
                },
                mcp_grant: None,
            },
            &crate::RecoveryOperation::empty(),
        )
        .unwrap();
    let (token, entry) = emitted(&machine);
    assert!(matches!(entry.record, Record::TurnSpawned { turn: actual, .. } if actual == turn));
    let at = Lsn(machine.world.last_applied_lsn.unwrap().0 + 1);
    scoped_handle(
        &mut machine,
        crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(at, (at, entry), None).unwrap(),
        },
        |_| panic!("the consumed claim has no active observation"),
    );
    assert!(matches!(
        machine
            .world
            .recovery_projection()
            .unwrap()
            .0
            .claim_receipt(key.claim())
            .unwrap()
            .state,
        RecoveryClaimState::Consumed { .. }
    ));
    assert!(
        machine
            .recovery_new_context(agent, &crate::RecoveryOperation::empty())
            .unwrap()
            .is_none()
    );
    let row = machine.world.agents[&agent].clone();
    let (next, _) = machine
        .prepare_new_issue(
            agent,
            &row,
            "ordinary next context",
            &crate::RecoveryOperation::empty(),
        )
        .unwrap();
    assert_ne!(next, turn);
}

#[test]
fn conflicting_duplicate_live_and_loss_answers_hold_without_invalidation() {
    let (mut machine, agent, key) = browser(true);
    let mut first = true;
    with_recovery_operation(
        &[key.clone(), key.clone()],
        |_| {
            if std::mem::take(&mut first) {
                RecoveryAvailability::Live
            } else {
                RecoveryAvailability::EstablishedUnavailable
            }
        },
        |operation| {
            assert!(!machine.recovery_admission_allowed(agent, operation));
            machine.handle(crate::Input::Tick, operation).unwrap();
        },
    );
    assert!(
        !machine
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Persist { .. } | Effect::Spawn { .. }))
    );
}

#[test]
fn replayed_try_recorded_issue_requires_new_repeat_consent_despite_live_old_authority() {
    let (mut current, agent, original, mut prefix) = authorize_kind(AuthorizationKind::TryRecorded);
    assert!(matches!(
        original.authorization().plan,
        RecoveryPlan::New {
            context: RecoveryContext::TryRecorded { .. },
            ..
        }
    ));
    let driven = drive_live(&mut current);
    assert_eq!(driven.spawned.len(), 1);
    let turn = driven.spawned[0];
    prefix.extend(driven.records);
    let mut restarted = replay_machine(&prefix);
    let row = restarted.world.turns[&turn].clone();
    let work = RecoveryWork {
        agent,
        goal: row.goal.unwrap(),
        turn,
    };
    let key = keys_at(restarted.world())
        .into_iter()
        .find(|key| key.claim() == original.claim())
        .unwrap();
    assert!(!restarted.live_issues.contains(&turn));
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(!restarted.recovery_launch_allowed(agent, turn, operation));
            restarted.reissue_turn(turn, agent, &row.task, row.model_selection, operation);
        },
    );
    let (token, entry) = emitted(&restarted);
    assert!(matches!(&entry.record, Record::RecoveryRequired(required)
        if required.predecessor == Some(key.claim()) && required.binding.work == work && required.reason == RecoveryReason::MissingSpawn));
    let at = Lsn(restarted.world.last_applied_lsn.unwrap().0 + 1);
    scoped_handle(
        &mut restarted,
        crate::Input::Persisted {
            token,
            append: RecordedAppend::try_new(at, (at, entry), None).unwrap(),
        },
        |_| RecoveryAvailability::Live,
    );
    let (state, _) = restarted.world.recovery_projection().unwrap();
    assert!(matches!(
        state.claim_receipt(key.claim()).unwrap().state,
        RecoveryClaimState::Superseded { .. }
    ));
    assert!(state.current(work).unwrap().authorization_lsn.is_none());
    let opportunity = restarted.recovery_opportunity(agent).unwrap();
    assert_eq!(opportunity.required.reason, RecoveryReason::MissingSpawn);
    assert!(!opportunity.recorded_available);
    with_recovery_operation(
        std::slice::from_ref(&key),
        |_| RecoveryAvailability::Live,
        |operation| {
            assert!(!restarted.recovery_launch_allowed(agent, turn, operation));
            assert!(restarted.recovery_new_context(agent, operation).is_err());
        },
    );
}

fn two_current_claims() -> (Machine, Vec<AgentId>) {
    let mut harness = Harness::new();
    harness.launch = false;
    let agents = vec![
        harness.hire_with_goal(&["false"]).0,
        harness.hire_with_goal(&["false"]).0,
    ];
    let mut prefix: Vec<_> = harness
        .journal
        .into_iter()
        .zip(1u64..)
        .map(|(entry, at)| (Lsn(at), entry))
        .collect();
    let mut prototype = replay_machine(&prefix);
    for (agent, (start, tag)) in agents.iter().zip([(9_000, 61), (10_000, 71)]) {
        historical_browser_claim_at(&mut prototype, &mut prefix, *agent, start, tag);
    }
    let authorizations: Vec<_> = prefix
        .iter()
        .filter_map(|(_, entry)| match &entry.record {
            Record::RecoveryAuthorized(authorization) => Some((**authorization).clone()),
            _ => None,
        })
        .collect();
    assert_eq!(authorizations.len(), 2);
    prefix.retain(|(_, entry)| !matches!(entry.record, Record::RecoveryAuthorized(_)));
    let mut machine = replay_machine(&prefix);
    scoped_handle(&mut machine, crate::Input::Started, |_| {
        panic!("the two unclaimed episodes have no authority")
    });
    let cleanup = drive_live(&mut machine);
    assert!(cleanup.spawned.is_empty());
    for authorization in authorizations {
        let at = machine.world.last_applied_lsn.unwrap().0 + 1;
        replay_record(
            &mut machine,
            &mut prefix,
            at,
            Record::RecoveryAuthorized(Box::new(authorization)),
        );
    }
    for key in keys_at(machine.world()) {
        let RecoverySubjectUse::DispatchIssued(issue) = key.purpose() else {
            unreachable!()
        };
        machine.live_issues.insert(issue.issued.work.turn);
    }
    (machine, agents)
}

#[test]
fn one_started_invocation_has_independent_facts_for_two_current_claims() {
    for covered in [1, 2] {
        let (mut machine, agents) = two_current_claims();
        let keys = keys_at(machine.world());
        assert_eq!(keys.len(), 2);
        let mut observed = 0;
        with_recovery_operation(
            &keys[..covered],
            |_| {
                observed += 1;
                RecoveryAvailability::Live
            },
            |operation| {
                machine.handle(crate::Input::Started, operation).unwrap();
            },
        );
        assert_eq!(observed, covered);
        let sampled = machine
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::SampleInstructions { .. }))
            .count();
        assert_eq!(sampled, covered);
        if covered == 2 {
            let driven = drive_live(&mut machine);
            assert_eq!(driven.spawned.len(), 2);
            let owners: std::collections::BTreeSet<_> = driven
                .spawned
                .iter()
                .map(|turn| machine.world.turns[turn].agent)
                .collect();
            assert_eq!(owners, agents.into_iter().collect());
            assert!(
                !driven
                    .records
                    .iter()
                    .any(|(_, entry)| matches!(entry.record, Record::TurnIssued { .. }))
            );
        }
    }
}

fn current_issue_rejects_distinct_request(pending: bool) {
    let (mut machine, agent, key, _) = authorize_live(pending);
    let driven = drive_live(&mut machine);
    assert_eq!(driven.spawned.len(), 1);
    let turn = driven.spawned[0];
    let original = machine.world.turns[&turn].clone();
    let work = RecoveryWork {
        agent,
        goal: original.goal.unwrap(),
        turn,
    };
    assert!(machine.live_issues.contains(&turn));
    assert!(original.is_issued_not_spawned());
    let state = machine.world.recovery_projection().unwrap().0;
    let before = state.clone();
    let expected = RecoveryExpected {
        work,
        issue_lsn: original.issue_lsn,
        source_lsn: original.issue_lsn,
        episode: state.current(work).map(|episode| episode.id),
        predecessor: Some(key.claim()),
        control_lsn: machine.world.agents[&agent].control_lsn,
        ticket: key
            .authorization()
            .binding
            .ticket
            .map(|ticket| (ticket.card, ticket.round)),
    };
    let hops = machine.world.goals[&work.goal].turns_on_goal;
    let spend = format!("{:?}", machine.spend);
    let drawn = machine.rng.drawn();
    let inspected = machine.recovery_opportunity(agent);
    let handled = machine.recover(
        RecoveryCommand {
            request_id: Hex32::new([111; 32]),
            digest: Hex32::new([112; 32]),
            expected,
            choice: RecoveryChoice::RepeatIssuedFresh,
            attribution: dudes_proto::recovery::RecoveryAttribution::LocalOperator {
                pid: 17,
                uid: None,
            },
        },
        &crate::RecoveryOperation::empty(),
    );
    assert!(
        matches!(inspected, Err(RecoveryRefusal::NotRecoverable)),
        "inspection={inspected:?}; refusal={:?}; effects={:?}",
        handled.recovery().unwrap().refusal,
        machine.effects()
    );
    assert_eq!(
        handled.recovery().unwrap().refusal,
        Some(RecoveryRefusal::NotRecoverable)
    );
    assert!(machine.effects().is_empty());
    assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
    let after = &machine.world.turns[&turn];
    assert_eq!(
        (&after.task, &after.answers, after.issue_lsn),
        (&original.task, &original.answers, original.issue_lsn)
    );
    assert_eq!(machine.world.goals[&work.goal].turns_on_goal, hops);
    assert_eq!(format!("{:?}", machine.spend), spend);
    assert_eq!(machine.rng.drawn(), drawn);
    let receipt = machine
        .world
        .recovery_projection()
        .unwrap()
        .0
        .receipt(
            key.authorization().request_id,
            key.authorization().request_digest,
        )
        .unwrap()
        .unwrap();
    assert_eq!(receipt.claim, key.claim());
    assert!(matches!(receipt.state, RecoveryClaimState::IssueBound(_)));
}

#[test]
fn current_new_issue_refuses_distinct_inspect_and_submit_without_mutation() {
    current_issue_rejects_distinct_request(true);
}

#[test]
fn current_repeat_issue_refuses_distinct_inspect_and_submit_without_mutation() {
    current_issue_rejects_distinct_request(false);
}

#[test]
fn delayed_baseline_issue_and_spawn_arm_each_require_their_own_fresh_facts() {
    for denied in [
        dudes_proto::RecordKind::InstructionObservationStarted,
        dudes_proto::RecordKind::TurnIssued,
        dudes_proto::RecordKind::HookTokenMinted,
    ] {
        let (mut machine, _, key, _) = authorize_live(true);
        let driven = drive_until_hold(&mut machine, Some(denied));
        assert!(
            driven
                .records
                .iter()
                .any(|(_, entry)| entry.record.kind() == denied)
        );
        assert!(driven.spawned.is_empty());
        assert!(!machine.effects().iter().any(|effect| matches!(
            effect,
            Effect::Spawn { .. }
                | Effect::Persist {
                    entry: Entry {
                        record: Record::RecoveryRequired(_)
                            | Record::RecoveryInvalidated(_)
                            | Record::TurnEnded { .. },
                        ..
                    },
                    ..
                }
        )));
        let receipt = machine
            .world
            .recovery_projection()
            .unwrap()
            .0
            .claim_receipt(key.claim())
            .unwrap();
        if denied == dudes_proto::RecordKind::InstructionObservationStarted {
            assert_eq!(receipt.state, &RecoveryClaimState::Pending);
        } else {
            assert!(matches!(receipt.state, RecoveryClaimState::IssueBound(_)));
        }
    }
}

#[test]
fn mcp_grant_acknowledgment_forwards_live_facts_and_holds_on_uncertainty() {
    for held in [false, true] {
        let (mut machine, _, key, _) = authorize_kind(AuthorizationKind::Mcp);
        let driven = drive_until_hold(
            &mut machine,
            held.then_some(dudes_proto::RecordKind::McpLaunchGranted),
        );
        assert!(
            driven
                .records
                .iter()
                .any(|(_, entry)| matches!(entry.record, Record::McpLaunchGranted { .. }))
        );
        assert_eq!(driven.spawned.len(), usize::from(!held));
        assert!(matches!(
            machine
                .world
                .recovery_projection()
                .unwrap()
                .0
                .claim_receipt(key.claim())
                .unwrap()
                .state,
            RecoveryClaimState::IssueBound(_)
        ));
        assert!(
            !machine
                .effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Spawn { .. }))
        );
    }
}

#[test]
fn unavailable_pending_claim_invalidates_but_indeterminate_or_missing_facts_do_not() {
    for mode in 0..3 {
        let (mut machine, _, key) = browser(true);
        let before = machine.world.recovery_projection().unwrap().0.clone();
        if mode == 0 {
            machine
                .handle(crate::Input::Tick, &crate::RecoveryOperation::empty())
                .unwrap();
        } else {
            scoped_handle(&mut machine, crate::Input::Tick, |_| {
                if mode == 1 {
                    RecoveryAvailability::Indeterminate
                } else {
                    RecoveryAvailability::EstablishedUnavailable
                }
            });
        }
        let invalidations = machine
            .effects()
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    Effect::Persist {
                        entry: Entry {
                            record: Record::RecoveryInvalidated(_),
                            ..
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(invalidations, usize::from(mode == 2));
        assert_eq!(machine.world.recovery_projection().unwrap().0, &before);
        if mode == 2 {
            let (token, entry) = emitted(&machine);
            let lsn = Lsn(10_000);
            scoped_handle(
                &mut machine,
                crate::Input::Persisted {
                    token,
                    append: RecordedAppend::try_new(lsn, (lsn, entry), None).unwrap(),
                },
                |_| RecoveryAvailability::Live,
            );
            let state = machine.world.recovery_projection().unwrap().0;
            assert!(matches!(
                state.claim_receipt(key.claim()).unwrap().state,
                RecoveryClaimState::Invalidated { .. }
            ));
        }
        assert!(
            !machine
                .effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Spawn { .. }))
        );
    }
}
