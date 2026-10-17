//! Closing acknowledgement and work admission through the production daemon.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_rpc::Client;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

fn operator(settings: &duded::Settings) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential"),
    )
    .expect("credential shape")
}

#[test]
fn legacy_announce_answers_strict_empty_and_journals_announcement() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let mut client = Client::connect(settings.socket()).expect("connect");
    let reply = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({})),
            OperatorCredential::new(credential).expect("credential shape"),
        )
        .expect("announcement response");
    drop(client);
    running.stop(ExitCause::ShutdownRpc);
    assert_eq!(reply.result, Some(serde_json::json!({})));
    assert!(reply.error.is_none());
    assert!(fixture.entries().iter().any(|(_, entry)| matches!(
        entry.record,
        Record::ClosingTime {
            phase: dudes_proto::ClosingPhase::Announced
        }
    )));
}

#[test]
fn a_durable_unfolded_announcement_fences_later_goal_mutation() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .expect("boot");
    daemon
        .onboarding_failure()
        .arm(duded::runtime::OnboardingFailurePoint::ClosingAnnounced);
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect");
    let failed = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .expect("failed announcement response");
    assert_eq!(failed.error.map(|error| error.code), Some(-32603));
    let before = fixture.entries();
    assert!(before.iter().any(|(_, entry)| matches!(
        entry.record,
        Record::ClosingTime {
            phase: dudes_proto::ClosingPhase::Announced
        }
    )));
    let refused = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            operator(&settings),
        )
        .expect("later refusal");
    assert_eq!(refused.error.map(|error| error.code), Some(-32004));
    assert_eq!(fixture.entries(), before);
    let read = client
        .call_as_operator(Method::AgentList, None, operator(&settings))
        .expect("roster remains readable");
    assert_eq!(read.result, Some(serde_json::json!([])));
    drop(client);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn a_closing_short_write_keeps_the_initiating_error_and_stops_later_mutation() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .expect("boot");
    daemon.append_failure().short_write_next(5);
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect");
    let failed = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .expect("failed announcement response");
    assert_eq!(failed.error.map(|error| error.code), Some(-32603));
    let later = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            operator(&settings),
        )
        .expect("stopped writer response");
    assert_eq!(later.error.map(|error| error.code), Some(-32006));
    let read = client
        .call_as_operator(Method::AgentList, None, operator(&settings))
        .expect("roster remains readable");
    assert_eq!(read.result, Some(serde_json::json!([])));
    drop(client);
    running.stop(ExitCause::ShutdownRpc);
}

/// A writer poisoned while a closure is already active admits no further append or spawn, and
/// the daemon goes on reporting the stopped journal exactly as it does outside closing.
///
/// This is the `IGNORE_POISON` mutant, covering both
/// `Ctx::pump_started`'s and `Runtime::closing_tick`'s own `self.ctx.writer.poisoned()` checks.
/// Removing just those two conditions does not go red on its own: the same poisoning append also
/// sets `Ctx::closing_failed_after` (`pump_started`'s tail arms it for any failure while a closure
/// is open, closing-origin or not), and that latch alone still stops `closing_tick` the next time
/// it runs. Killing this mutant means removing the poison checks *and* neutralising that latch, at
/// which point a correct `closing_tick` has nothing left to notice the poison with: it proceeds
/// into `closing_finish`'s append, the journal's own independent poisoned check refuses it, and
/// the resulting `Err` propagates out of `closing_tick` through `serve`'s `?` and crashes the
/// daemon thread instead of returning `Ok(false)`. The one live participant's turn stays open and
/// unacknowledged, so without the clock advance below, `closing_tick` would return early on
/// `!session.ready()` regardless of poisoning and the mutant would go uncaught; advancing past the
/// grace makes it `expired` instead, the one condition under which a correct `closing_tick` still
/// has to notice the poison itself rather than bail for an unrelated reason.
#[test]
fn a_poisoned_writer_during_an_active_closure_admits_no_further_mutation() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let clock = Arc::new(TestClock::new());
    let mut daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            duded::startup::Witness::default(),
        ),
    )
    .expect("boot");
    daemon.closing_grace_for_test(Duration::from_millis(500));
    let append_failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("the exact closing participant actually spawned", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    let mut client = Client::connect(settings.socket()).expect("connect");
    let announced = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .expect("announcement response");
    assert!(announced.error.is_none(), "{announced:?}");

    // Poison the writer with an ordinary, unrelated append while the closure stays open: the
    // one participant's turn is still unfinished, so closing itself has written nothing more
    // since the announcement above.
    append_failure.short_write_next(5);
    let poisoning = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            operator(&settings),
        )
        .expect("the poisoning call still answers");
    assert_eq!(
        poisoning.error.as_ref().map(|error| error.code),
        Some(-32603),
        "the initiating failure keeps its original error: {poisoning:?}"
    );
    let after_poison = fixture.entries();

    // Past the grace deadline, a correct `closing_tick` would otherwise be ready to call
    // `closing_finish`; the poisoned writer is what has to stop it, each `serve` pass, with no
    // RPC caller in the loop to pre-check anything.
    clock.advance(600);
    running.handle.input(Input::Tick);

    let roster = client
        .call_as_operator(Method::AgentList, None, operator(&settings))
        .expect("the daemon answers reads after the poison instead of having crashed");
    assert!(roster.error.is_none(), "{roster:?}");
    let views: Vec<dudes_proto::projection::AgentView> =
        serde_json::from_value(roster.result.unwrap()).unwrap();
    assert_eq!(
        views.iter().map(|view| view.id).collect::<Vec<_>>(),
        [agent],
        "the second hire never persisted"
    );

    let retried = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .expect("a later closing call still answers");
    assert_eq!(
        retried.error.as_ref().map(|error| error.code),
        Some(dudes_proto::rpc::JOURNAL_STOPPED_CODE),
        "closing is refused the same stopped-journal way as any other mutation: {retried:?}"
    );

    assert_eq!(
        fixture.entries().len(),
        after_poison.len(),
        "closing made no further append once the writer was poisoned"
    );
    assert!(
        !fixture.records().iter().any(|record| matches!(
            record,
            Record::ClosingTime {
                phase: ClosingPhase::Complete
            }
        )),
        "a poisoned writer cannot carry closing to completion"
    );
    drop(client);
    running.stop(ExitCause::ShutdownRpc);
}

use dudes_journal::{Fsync, JournalOptions};
use dudes_proto::{ClosingPhase, Entry, ExitStatus, LoopDecision, TurnOutcome};
use dudes_supervisor::{Input, World, testing::Harness};
use dudes_types::{AgentId, Lsn, Timestamp, TurnId};
use std::collections::BTreeMap;

type SeedRecords = Vec<(Lsn, Entry)>;

fn saving_source(fixture: &Fixture) -> (AgentId, SeedRecords, SeedRecords, Entry) {
    let source = Fixture::new();
    let settings = source.settings();
    let host = SharedHost::with_native_evidence(source.journal_dir());
    let seams = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        duded::startup::Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let mut running = common::start(boot(&source.settings(), seams).unwrap());
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let (old_turn, spawn_lsn) = loop {
        let found = source
            .entries()
            .into_iter()
            .find_map(|(lsn, entry)| match entry.record {
                Record::TurnSpawned {
                    agent: owner, turn, ..
                } if owner == agent => Some((turn, lsn)),
                _ => None,
            });
        if let Some(found) = found {
            break found;
        }
        source.await_record("the preceding owned turn actually spawned", |record| {
            matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent)
                .then_some(())
        });
    };
    let base: Vec<_> = source
        .entries()
        .into_iter()
        .filter(|(lsn, _)| *lsn <= spawn_lsn)
        .collect();
    let template = base
        .iter()
        .find_map(|(_, entry)| {
            matches!(entry.record,
        Record::TurnIssued { turn, .. } if turn == old_turn)
            .then_some(entry.clone())
        })
        .unwrap();
    running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    common::end_turn_with_native(&source, &running.handle, &host, old_turn);
    source.await_record("the preceding owned comparison completed", |record| {
        matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. }, ..
        } if *turn == old_turn)
        .then_some(())
    });
    let ended = source
        .entries()
        .into_iter()
        .filter(|(_, entry)| match entry.record {
            Record::TurnEnded { turn, .. }
            | Record::InstructionObservationFinished {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. },
                ..
            }
            | Record::InstructionAttemptBoundarySettled {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. },
                ..
            } => turn == old_turn,
            _ => false,
        })
        .collect::<Vec<_>>();
    assert!(base.iter().any(|(_, entry)| matches!(entry.record,
        Record::InstructionObservationStarted { owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. }, .. }
            if turn == old_turn)));
    assert!(ended.iter().any(|(_, entry)| matches!(entry.record,
        Record::InstructionAttemptBoundarySettled { owner: dudes_proto::InstructionObservationOwner::Managed { turn, .. }, .. }
            if turn == old_turn)));
    running.stop(ExitCause::Ended);
    (agent, base, ended, template)
}

fn seed_saving_prefix(fixture: &Fixture) -> (Lsn, TurnId, String) {
    let (agent, base, ended, template) = saving_source(fixture);
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        JournalOptions {
            fsync: Fsync::Full,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let mut world = World::default();
    let mut positions = BTreeMap::new();
    for (old_lsn, mut entry) in base {
        remap_seed_references(&mut entry.record, &positions);
        let lsn = writer.append(&entry).unwrap();
        world.apply(lsn, &entry);
        positions.insert(old_lsn, lsn);
    }
    let announced = Entry {
        at: Timestamp::from_unix_millis(17),
        record: Record::ClosingTime {
            phase: ClosingPhase::Announced,
        },
    };
    let announcement = writer.append(&announced).unwrap();
    world.apply(announcement, &announced);
    for (old_lsn, mut entry) in ended {
        remap_seed_references(&mut entry.record, &positions);
        let lsn = writer.append(&entry).unwrap();
        world.apply(lsn, &entry);
        positions.insert(old_lsn, lsn);
    }
    let goal = world.agents[&agent].goal.unwrap();
    let ordinary = world
        .loop_input(
            agent,
            dudes_supervisor::SpendView::default(),
            dudes_policy::WorkInput::default(),
            None,
            dudes_proto::Spend::default(),
        )
        .unwrap();
    let input = dudes_policy::closing_input(announcement, agent, goal, ordinary);
    let dudes_policy::ClosingDecided::Policy(decided) = dudes_policy::decide_closing(&input) else {
        panic!("fixture admits saving");
    };
    let LoopDecision::Continue { task } = &decided.decision else {
        panic!("fixture continues");
    };
    let task = task.clone();
    let decision = Entry {
        at: Timestamp::from_unix_millis(18),
        record: Record::LoopDecided {
            agent,
            input_digest: dudes_supervisor::closing_input_digest(&input).unwrap(),
            decision: decided.decision,
            gate_trace: decided.gate_trace,
        },
    };
    let decision_lsn = writer.append(&decision).unwrap();
    world.apply(decision_lsn, &decision);
    let saving = TurnId::from_parts(12, 13);
    let mut issued = template;
    if let Record::TurnIssued {
        turn,
        task: recorded,
        lsn_of_decision,
        answers,
        ..
    } = &mut issued.record
    {
        *turn = saving;
        recorded.clone_from(&task);
        *lsn_of_decision = decision_lsn;
        *answers = decided.answers;
    }
    writer.append(&issued).unwrap();
    drop(writer);
    (announcement, saving, task)
}

fn remap_seed_references(record: &mut Record, positions: &BTreeMap<Lsn, Lsn>) {
    let remap = |position: &mut Lsn| {
        *position = *positions
            .get(position)
            .expect("the earlier owned source record");
    };
    match record {
        Record::TurnIssued {
            lsn_of_decision, ..
        } => remap(lsn_of_decision),
        Record::InstructionObservationFinished {
            baseline,
            settlement,
            ..
        } => {
            let dudes_proto::InstructionBaselineRef::Started { lsn } = baseline else {
                panic!("this seed compares sampled managed attempts");
            };
            remap(lsn);
            let end = match settlement {
                dudes_proto::InstructionSettlement::Compared { end, .. } => Some(end),
                dudes_proto::InstructionSettlement::Unknown { end, .. } => end.as_mut(),
                dudes_proto::InstructionSettlement::NoProcess { proof_lsn, .. } => {
                    remap(proof_lsn);
                    None
                }
                dudes_proto::InstructionSettlement::NoIssuedTurn => None,
            };
            if let Some(end) = end {
                let dudes_proto::InstructionEndBoundary::ManagedTurn { end_lsn, .. } = end else {
                    panic!("this seed ends managed children");
                };
                remap(end_lsn);
            }
        }
        Record::InstructionAttemptBoundarySettled {
            started_lsn, proof, ..
        } => {
            remap(started_lsn);
            match proof {
                dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt,
                }
                | dudes_proto::InstructionBoundaryAbsenceProof::PriorRunGroupAbsent {
                    receipt,
                    ..
                } => {
                    let lsn = match receipt {
                        dudes_proto::InstructionNativeBoundaryReceipt::Spawned { lsn }
                        | dudes_proto::InstructionNativeBoundaryReceipt::LaunchFailureEnded {
                            lsn,
                        } => lsn,
                    };
                    remap(lsn);
                }
                dudes_proto::InstructionBoundaryAbsenceProof::PriorBootAbsent { .. } => {
                    panic!("this seed proves its retained native boundary directly");
                }
            }
        }
        _ => {}
    }
}

#[test]
fn a_durable_saving_turn_reissues_once_before_conservative_restart_completion() {
    let fixture = Fixture::new();
    let (announcement, saving, task) = seed_saving_prefix(&fixture);
    assert_replayed_saving(&fixture, announcement, saving, task);
}

fn assert_replayed_saving(fixture: &Fixture, announcement: Lsn, saving: TurnId, task: String) {
    let settings = fixture.settings();
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .expect("replay boot");
    let mut running = common::start(daemon);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    let entries = fixture.entries();
    let issues: Vec<_> = entries
        .iter()
        .filter_map(|(lsn, entry)| match &entry.record {
            Record::TurnIssued { turn, task, .. } if *lsn > announcement => {
                Some((*turn, task.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(issues, [(saving, task)]);
    let spawns: Vec<_> = entries.iter().filter_map(|(lsn, entry)| matches!(entry.record, Record::TurnSpawned { turn, reissued: true, .. } if turn == saving).then_some(*lsn)).collect();
    assert_eq!(spawns.len(), 1);
    let complete = entries
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::ClosingTime {
                    phase: ClosingPhase::Complete
                }
            )
            .then_some(*lsn)
        })
        .unwrap();
    assert!(spawns[0] < complete);
    assert_eq!(host.shutdowns(), 1);
}

fn seed_done_during_closing(fixture: &Fixture) -> AgentId {
    let mut source = Harness::new();
    let (agent, goal) = source.hire_with_goal(&["/usr/bin/true"]);
    let turn = source.supervisor.world().agents[&agent].open_turn.unwrap();
    let announced_at = source.journal.len();
    source.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    source.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: dudes_proto::goal::Evidence::Command {
            argv: vec!["/usr/bin/true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    let mut journal: Vec<_> = source
        .journal
        .into_iter()
        .zip(1u64..)
        .map(|(entry, lsn)| (Some(Lsn(lsn)), entry))
        .collect();
    journal.insert(
        announced_at,
        (
            None,
            Entry {
                at: Timestamp::from_unix_millis(17),
                record: Record::ClosingTime {
                    phase: ClosingPhase::Announced,
                },
            },
        ),
    );
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        JournalOptions {
            fsync: Fsync::Full,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let mut positions = BTreeMap::new();
    for (old_lsn, mut entry) in journal {
        remap_seed_references(&mut entry.record, &positions);
        let lsn = writer.append(&entry).unwrap();
        if let Some(old_lsn) = old_lsn {
            positions.insert(old_lsn, lsn);
        }
    }
    agent
}

#[test]
fn a_restarted_closure_whose_only_participant_finished_done_reports_it_settled() {
    use dudes_proto::rpc::closing::{ClosingReport, ClosingSettled, ClosingSettledReason};
    let fixture = Fixture::new();
    let agent = seed_done_during_closing(&fixture);
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .expect("replay boot");
    let mut running = common::start(daemon);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    let path = std::fs::read_dir(&settings.runtime_dir)
        .unwrap()
        .map(|item| item.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".closing.json"))
        .expect("the final report");
    let report: ClosingReport = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(report.participants, [agent]);
    assert_eq!(report.missing, [agent]);
    assert_eq!(
        report.settled,
        [ClosingSettled {
            agent,
            reason: ClosingSettledReason::GoalDone
        }]
    );
}

#[test]
fn a_completed_closure_is_historical_on_a_later_normal_boot() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).unwrap();
    let reply = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .unwrap();
    assert!(reply.error.is_none());
    drop(client);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    let complete = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::ClosingTime {
                    phase: ClosingPhase::Complete
                }
            )
            .then_some(lsn)
        })
        .unwrap();

    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("the exact closing participant actually spawned", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    assert!(fixture.entries().iter().any(|(lsn, entry)| {
        *lsn > complete
            && matches!(entry.record, Record::TurnSpawned { agent: owner, .. } if owner == agent)
    }));
    assert_eq!(
        host.shutdowns(),
        0,
        "a historical Complete cannot stop this run"
    );
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn final_report_loss_does_not_rewrite_durable_completion_or_the_hard_receipt() {
    use dudes_proto::rpc::closing::ClosingStarted;
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let mut daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            duded::startup::Witness::default(),
        ),
    )
    .unwrap();
    daemon.closing_grace_for_test(std::time::Duration::from_millis(500));
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("the exact closing participant actually spawned", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    let mut client = Client::connect(settings.socket()).unwrap();
    let reply = client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::json!({"action":"announce"})),
            operator(&settings),
        )
        .unwrap();
    assert!(reply.error.is_none());
    let started: ClosingStarted = serde_json::from_value(reply.result.unwrap()).unwrap();
    assert_eq!(started.participants, [agent]);
    std::fs::create_dir(&started.report.path).unwrap();
    drop(client);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    assert!(
        started.report.path.is_dir(),
        "the report sink actually failed"
    );
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::ClosingTime {
            phase: ClosingPhase::Complete
        }
    )));
    assert_eq!(
        std::fs::read_to_string(started.completion.path).unwrap(),
        format!("complete {}\n", started.completion.run)
    );
    assert_eq!(host.shutdowns(), 1);
}

#[test]
fn announce_retries_keep_the_snapshot_and_original_clock_bound() {
    use dudes_proto::rpc::closing::ClosingStarted;
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let clock = Arc::new(TestClock::new());
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            duded::startup::Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("the exact closing participant actually spawned", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    let mut client = Client::connect(settings.socket()).unwrap();
    let mut announce = || {
        let reply = client
            .call_as_operator(
                Method::SystemClosingTime,
                Some(serde_json::json!({"action":"announce"})),
                operator(&settings),
            )
            .unwrap();
        assert!(reply.error.is_none());
        serde_json::from_value::<ClosingStarted>(reply.result.unwrap()).unwrap()
    };
    let first = announce();
    clock.advance(359_000);
    let retried = announce();
    assert_eq!(first, retried);
    assert_eq!(first.participants, [agent]);
    clock.advance(2_000);
    drop(client);
    running.handle.input(Input::Tick);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::ClosingTime {
                    phase: ClosingPhase::Announced
                }
            ))
            .count(),
        1
    );
}

#[cfg(feature = "fault-injection")]
mod before_spawn_crash {
    use super::*;
    use std::path::Path;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    const STATE: &str = "DUDES_CLOSING_CRASH_STATE";
    const RUNTIME: &str = "DUDES_CLOSING_CRASH_RUNTIME";
    const MARKER: &str = "DUDES_CLOSING_CRASH_MARKER";

    struct OwnedDaemon(Child);

    impl Drop for OwnedDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn closing_crash_daemon_helper() {
        let Some(state) = std::env::var_os(STATE) else {
            return;
        };
        let runtime = std::env::var_os(RUNTIME).unwrap();
        let marker = std::env::var_os(MARKER).unwrap();
        let settings = common::settings_at(Path::new(&state), Path::new(&runtime));
        let daemon = boot(
            &settings,
            seams(
                &settings,
                duded::fault::StallBeforeSpawn::new(SharedHost::new(), 1, marker.into()),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                duded::startup::Witness::default(),
            ),
        )
        .unwrap();
        let mut running = common::start(daemon);
        running.join();
        panic!("the before-spawn fault did not park");
    }

    #[test]
    #[allow(
        clippy::disallowed_types,
        reason = "the test owns, kills and reaps one crash-cut daemon subprocess"
    )]
    fn a_real_before_spawn_crash_reissues_the_same_durable_saving_turn_once() {
        let fixture = Fixture::new();
        let (announcement, saving, task) = seed_saving_prefix(&fixture);
        let marker = fixture.state.path().join("closing-before-spawn.marker");
        let mut child = OwnedDaemon(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "before_spawn_crash::closing_crash_daemon_helper",
                    "--quiet",
                ])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", fixture.state.path())
                .env(STATE, fixture.state.path())
                .env(RUNTIME, fixture.runtime.path())
                .env(MARKER, &marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.is_file() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "daemon exited before the cut"
            );
            assert!(Instant::now() < deadline, "daemon never reached the cut");
            std::thread::sleep(Duration::from_millis(10));
        }
        let entries = fixture.entries();
        assert!(entries.iter().any(|(_, entry)| matches!(entry.record,
            Record::TurnIssued { turn, .. } if turn == saving)));
        assert!(!entries.iter().any(|(_, entry)| matches!(entry.record,
            Record::TurnSpawned { turn, .. } if turn == saving)));
        assert!(!entries.iter().any(|(_, entry)| matches!(
            entry.record,
            Record::ClosingTime {
                phase: ClosingPhase::Complete
            }
        )));
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        drop(child);
        assert_replayed_saving(&fixture, announcement, saving, task);
    }
}
