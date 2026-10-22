use super::*;
use crate::runtime::Witness;
use crate::runtime::work_test_support as common;
use std::sync::Arc;

mod authority_tests;
mod ticket_context;

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn empty(fixture: &common::Fixture) -> TestDaemon {
    let settings = fixture.settings();
    crate::boot(
        &settings,
        common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap()
}

#[test]
fn request_only_absence_is_inert_and_names_the_checked_cut() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let before = daemon.ctx.projections.export().unwrap();
    let entries = fixture.entries();
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let through = daemon.ctx.projections.last_applied_lsn();
    let request = Hex32::new([71; 32]);
    for _ in 0..2 {
        assert_eq!(
            daemon.ctx.recovery_read(request),
            RecoveryReceiptResult::Absent {
                checked_through: through
            }
        );
    }
    assert_eq!(daemon.ctx.projections.export().unwrap(), before);
    assert_eq!(fixture.entries(), entries);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(daemon.sup.effects().is_empty());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn lookup_refuses_raw_journal_advancement_without_certifying_absence() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let request = Hex32::new([72; 32]);
    assert!(matches!(
        daemon.ctx.recovery_read(request),
        RecoveryReceiptResult::Absent { .. }
    ));
    let before = daemon.ctx.projections.export().unwrap();
    let position = daemon.ctx.writer.next_lsn().unwrap();
    let entry = dudes_proto::Entry {
        at: dudes_types::Timestamp::UNIX_EPOCH,
        record: dudes_proto::Record::Checkpoint {
            projection_version: 51,
            segment: position.segment(),
            offset: position.offset(),
        },
    };
    daemon.ctx.writer.append(&entry).unwrap();
    let entries = fixture.entries();
    assert_eq!(
        daemon.ctx.recovery_read(request),
        RecoveryReceiptResult::Unavailable {
            reason: RecoveryReadUnavailable::StaleCut
        }
    );
    assert_eq!(daemon.ctx.projections.export().unwrap(), before);
    assert_eq!(fixture.entries(), entries);
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn only_preappend_refusals_are_known_not_to_have_written() {
    let clean = anyhow::Error::new(dudes_journal::Error::RecoveryAdmission {
        kind: dudes_proto::RecordKind::RecoveryAuthorized,
        reason: RecoveryStateError::Control,
    });
    assert_eq!(known_no_write(&clean), Some(RecoveryRefusal::StaleControl));
    let durable = anyhow::Error::new(dudes_journal::Error::RecoveryRecord {
        lsn: dudes_types::Lsn(17),
        reason: RecoveryStateError::Control,
    });
    assert_eq!(known_no_write(&durable), None);
    let uncertain = anyhow::Error::new(dudes_journal::Error::RecoveryStore(
        dudes_journal::error::RecoveryStoreError::JournalUncertain,
    ));
    assert_eq!(known_no_write(&uncertain), None);
}

fn operator_admission() -> OperatorAdmission {
    let (sent, received) = std::sync::mpsc::channel();
    let mut dispatcher = dudes_rpc::Dispatcher::new(dudes_rpc::PeerClassifier::new(
        dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap(),
        Arc::new(crate::classify::WorldTokens::new()),
    ));
    dispatcher.register_current_operator(
        dudes_proto::rpc::CurrentMethod::Legacy(dudes_proto::rpc::Method::SystemPing),
        move |_, admission| {
            sent.send(admission).unwrap();
            Ok(serde_json::Value::Null)
        },
    );
    let frame = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system.ping\",\"cred\":\"{}\"}}\n",
        "C".repeat(43)
    );
    dudes_rpc::server::serve_stream(
        std::io::Cursor::new(frame.into_bytes()),
        &Arc::new(dudes_rpc::Outbound::sink()),
        dudes_rpc::PeerIdentity {
            uid: Some(501),
            gid: None,
            pid: dudes_rpc::PeerPid::Reported {
                pid: 4242,
                pidversion: None,
            },
        },
        dudes_rpc::PeerProvenance::NoRecordedAgent,
        &dispatcher,
    )
    .unwrap();
    received.recv().unwrap()
}

fn append_history(daemon: &mut TestDaemon, record: dudes_proto::Record) {
    let entry = dudes_proto::Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let append = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let through = append.through();
    let (main, checkpoint) = append.into_parts();
    for (lsn, entry) in std::iter::once(main).chain(checkpoint) {
        daemon
            .sup
            .handle(
                dudes_supervisor::Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(through));
    assert!(daemon.sup.effects().is_empty());
}

fn ticket_owner(daemon: &mut TestDaemon, fixture: &common::Fixture) -> dudes_types::AgentId {
    use dudes_proto::record::samples;
    use dudes_proto::{Record, RecordKind};
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id,
        cwd,
        workspace,
        mcp_servers,
        ..
    } = &mut hired
    else {
        unreachable!()
    };
    let agent = *id;
    mcp_servers.clear();
    *cwd = fixture.state.path().to_path_buf();
    *workspace = Some(dudes_proto::record::RecordedWorkspace {
        id: "main".into(),
        label: "Main".into(),
    });
    append_history(daemon, hired);
    agent
}

fn recoverable_ticket(daemon: &mut TestDaemon, fixture: &common::Fixture) -> RecoveryCommand {
    let (owner, turn) = issued_ticket_with(daemon, fixture, |_, _, _, _| {});
    append_vanished_attempt(daemon, owner, turn);
    inspected_command(daemon, owner)
}

fn issued_ticket_with(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    before_issue: impl FnOnce(
        &mut TestDaemon,
        dudes_types::AgentId,
        dudes_types::GoalId,
        dudes_types::TurnId,
    ),
) -> (dudes_types::AgentId, dudes_types::TurnId) {
    use dudes_proto::record::samples;
    use dudes_proto::{Record, RecordKind};
    use dudes_types::{Seq, TurnId};
    let owner = ticket_owner(daemon, fixture);
    let created = samples::record(RecordKind::TicketCreated);
    let Record::TicketCreated {
        card, done_when, ..
    } = &created
    else {
        unreachable!()
    };
    let card = *card;
    let check = crate::ticket_index::goal_check(done_when);
    append_history(daemon, created);
    append_history(
        daemon,
        Record::TicketAssigned {
            card,
            agent: owner,
            by: dudes_proto::Actor::System,
            message: None,
            expected_seq: Seq(0),
        },
    );
    let mut started = samples::record(RecordKind::TicketStarted);
    let Record::TicketStarted {
        agent,
        goal,
        expected_seq,
        ..
    } = &mut started
    else {
        unreachable!()
    };
    *agent = owner;
    *expected_seq = Seq(1);
    goal.completion_check = check;
    let goal = goal.id;
    append_history(daemon, started);
    let decision = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::LoopDecided {
            agent: owner,
            input_digest: "ab".repeat(32),
            decision: dudes_proto::LoopDecision::Continue {
                task: "finish the recorded ticket".into(),
            },
            gate_trace: Vec::new(),
        },
    );
    let turn = TurnId::from_parts(915, 1);
    before_issue(daemon, owner, goal, turn);
    append_history(
        daemon,
        Record::TurnIssued {
            agent: owner,
            goal,
            turn,
            mode: dudes_types::Mode::Managed,
            task: "finish the recorded ticket".into(),
            session: None,
            model_selection: Some(dudes_types::ModelSelection::provider_default()),
            lsn_of_decision: decision,
            answers: Vec::new(),
        },
    );
    (owner, turn)
}

fn append_vanished_attempt(
    daemon: &mut TestDaemon,
    owner: dudes_types::AgentId,
    turn: dudes_types::TurnId,
) {
    use dudes_proto::record::samples;
    use dudes_proto::{Record, RecordKind};
    for kind in [RecordKind::TurnSpawned, RecordKind::TurnEnded] {
        let mut record = samples::record(kind);
        match &mut record {
            Record::TurnSpawned {
                agent,
                turn: issued,
                reissued,
                session,
                ..
            } => {
                *agent = owner;
                *issued = turn;
                *reissued = false;
                *session = None;
            }
            Record::TurnEnded {
                agent,
                turn: issued,
                outcome,
                exit,
                usage,
                session,
                session_usage,
                model_usage,
                last_message,
                ..
            } => {
                *agent = owner;
                *issued = turn;
                *outcome = Some(dudes_proto::TurnOutcome::Vanished);
                *exit = dudes_proto::ExitStatus::UNKNOWN;
                *usage = None;
                *session = None;
                *session_usage = None;
                model_usage.clear();
                *last_message = None;
            }
            _ => unreachable!(),
        }
        append_history(daemon, record);
    }
}

fn inspected_command(daemon: &mut TestDaemon, agent: dudes_types::AgentId) -> RecoveryCommand {
    let Answered::RecoveryInspected(RecoveryInspectResult::Available {
        expected,
        reason,
        choices,
        prior_effects_unknown,
        ..
    }) = daemon
        .ask(crate::runtime::Ask::RecoveryInspect {
            agent,
            admission: operator_admission(),
        })
        .unwrap()
    else {
        panic!("the original ticket must offer actual recovery")
    };
    assert_eq!(reason, dudes_proto::recovery::RecoveryReason::Vanished);
    assert_eq!(choices, RecoveryAllowedChoices::FreshOnly);
    assert!(prior_effects_unknown);
    assert_eq!(expected.ticket.as_ref().unwrap().round, 1);
    assert_eq!(
        daemon.ctx.recovery_read(Hex32::new([91; 32])),
        RecoveryReceiptResult::Absent {
            checked_through: daemon.ctx.projections.last_applied_lsn()
        }
    );
    RecoveryCommand {
        request_id: Hex32::new([91; 32]),
        digest: Hex32::new([92; 32]),
        expected: dudes_supervisor::input::RecoveryExpected {
            work: expected.work,
            issue_lsn: expected.issue_lsn,
            source_lsn: expected.source_lsn,
            episode: expected.episode,
            predecessor: expected.expected_predecessor,
            control_lsn: expected.control_lsn,
            ticket: expected.ticket.map(|ticket| (ticket.card, ticket.round)),
        },
        choice: dudes_supervisor::input::RecoveryChoice::FreshWithRecap,
        attribution: RecoveryAttribution::LocalOperator {
            pid: 4242,
            uid: Some(501),
        },
    }
}

fn assert_new_recovery_refused(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    command: RecoveryCommand,
    reason: RecoverySubmitRefusal,
) {
    use crate::runtime::Ask;
    let work = command.expected.work;
    let (card, round) = command.expected.ticket.unwrap();
    let entries = fixture.entries();
    let projection = daemon.ctx.projections.export().unwrap();
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let through = daemon.sup.world().last_applied_lsn;
    let hops = daemon.sup.world().goals[&work.goal].turns_on_goal;
    let completed = daemon.sup.world().completed_goal_lsn(work.goal);
    let original_card = daemon.ctx.projections.card(card).unwrap().unwrap();
    let spawned = daemon.ctx.host.spawned();
    assert_eq!(round, 1);
    assert_eq!(hops, 1);
    assert!(!entries.iter().any(|(_, entry)| matches!(
        &entry.record,
        dudes_proto::Record::RecoveryAuthorized(authorization)
            if authorization.request_id == command.request_id
    )));
    let Answered::RecoveryInspected(inspected) = daemon
        .ask(Ask::RecoveryInspect {
            agent: work.agent,
            admission: operator_admission(),
        })
        .unwrap()
    else {
        panic!("inspection response")
    };
    assert_eq!(
        inspected,
        RecoveryInspectResult::Unavailable {
            reason: RecoveryInspectUnavailable::NotRecoverable
        }
    );
    let Answered::RecoverySubmitted(submitted) = daemon
        .ask(Ask::Recover {
            command: Box::new(command),
            admission: operator_admission(),
        })
        .unwrap()
    else {
        panic!("submission response")
    };
    assert_eq!(
        submitted,
        RecoverySubmitResult::Refused {
            reason,
            prefix: RecoveryKnownPrefix {
                required: None,
                resume: None
            }
        }
    );
    assert_eq!(fixture.entries(), entries);
    assert_eq!(daemon.ctx.projections.export().unwrap(), projection);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert_eq!(daemon.sup.world().last_applied_lsn, through);
    assert_eq!(daemon.sup.world().agents[&work.agent].goal, Some(work.goal));
    assert_eq!(
        daemon.sup.world().goals[&work.goal].last_turn,
        Some(work.turn)
    );
    assert_eq!(daemon.sup.world().goals[&work.goal].turns_on_goal, hops);
    assert_eq!(daemon.sup.world().completed_goal_lsn(work.goal), completed);
    assert_eq!(daemon.sup.world().ticket_goal(work.goal), Some((card, 1)));
    assert_eq!(
        daemon.ctx.projections.card(card).unwrap().unwrap(),
        original_card
    );
    assert_eq!(daemon.ctx.host.spawned(), spawned);
    assert!(daemon.sup.effects().is_empty());
}

#[test]
fn active_closing_refuses_inspection_and_new_recovery_consent() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let command = recoverable_ticket(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    assert!(daemon.sup.is_closing());
    assert_eq!(daemon.sup.world().closing.as_ref().unwrap().complete, None);
    assert_new_recovery_refused(
        &mut daemon,
        &fixture,
        command,
        RecoverySubmitRefusal::Unavailable,
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn incomplete_closing_failure_refuses_inspection_and_new_recovery_consent() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let command = recoverable_ticket(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let failure = daemon
        .pump_started(
            &mut PumpReport::Discard,
            PumpOrigin::Closing,
            &[],
            |_, _operation| Err(anyhow::anyhow!("closing completion failed")),
        )
        .unwrap_err();
    assert_eq!(failure.to_string(), "closing completion failed");
    assert!(daemon.ctx.closing_failed_after.is_some());
    assert!(daemon.ctx.journal_writes_stopped());
    assert_eq!(daemon.sup.world().closing.as_ref().unwrap().complete, None);
    assert_new_recovery_refused(
        &mut daemon,
        &fixture,
        command,
        RecoverySubmitRefusal::Unavailable,
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn completed_and_ready_for_you_tickets_refuse_new_recovery_consent() {
    use dudes_proto::{Actor, Record};
    use dudes_types::{CardStatus, Provenance};

    for status in [CardStatus::Review, CardStatus::Done] {
        let fixture = common::Fixture::new();
        let mut daemon = empty(&fixture);
        let command = recoverable_ticket(&mut daemon, &fixture);
        let work = command.expected.work;
        let card = command.expected.ticket.unwrap().0;
        append_history(
            &mut daemon,
            Record::PredicateResult {
                goal: work.goal,
                turn: Some(work.turn),
                provenance: Provenance::Observed,
                pass: true,
                evidence: dudes_proto::Evidence::Command {
                    argv: vec!["cargo".into(), "test".into()],
                    program: None,
                    exit: Some(0),
                    output_tail: "ticket check passed".into(),
                },
            },
        );
        append_history(
            &mut daemon,
            Record::LoopDecided {
                agent: work.agent,
                input_digest: "cd".repeat(32),
                decision: dudes_proto::LoopDecision::Done,
                gate_trace: Vec::new(),
            },
        );
        let seq = daemon.ctx.projections.card(card).unwrap().unwrap().seq;
        append_history(
            &mut daemon,
            Record::CardTransition {
                card,
                from: CardStatus::Doing,
                to: CardStatus::Review,
                by: Actor::System,
                expected_seq: seq,
                reason: None,
            },
        );
        if status == CardStatus::Done {
            append_history(
                &mut daemon,
                Record::CardTransition {
                    card,
                    from: CardStatus::Review,
                    to: CardStatus::Done,
                    by: Actor::Peer {
                        pid: 4242,
                        uid: Some(501),
                    },
                    expected_seq: seq.next(),
                    reason: None,
                },
            );
        }
        assert_eq!(
            daemon.ctx.projections.card(card).unwrap().unwrap().status,
            status
        );
        assert!(daemon.sup.world().completed_goal_lsn(work.goal).is_some());
        assert_new_recovery_refused(
            &mut daemon,
            &fixture,
            command,
            RecoverySubmitRefusal::StaleWork,
        );
        let mut running = common::start(daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

#[test]
fn a_clean_refusal_drains_an_already_owed_durable_ack_before_returning() {
    use crate::runtime::{Ask, PumpOrigin, PumpReport};
    use dudes_proto::recovery::RecoveryWork;
    use dudes_supervisor::input::{RecoveryChoice, RecoveryExpected};
    use dudes_supervisor::{Command, Effect, Input};
    use dudes_types::{AgentId, GoalId, Lsn, TurnId};

    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("hired")
    };
    daemon
        .sup
        .handle(
            Input::Command(Command::Pause {
                agent: hired.agent,
                browser: None,
            }),
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    let effects = daemon.sup.take_effects();
    let [Effect::Persist { token, entry }] = effects.as_slice() else {
        panic!("actual pause append")
    };
    let append = daemon.ctx.append(daemon.sup.world(), entry).unwrap();
    let through = append.through();
    let command = RecoveryCommand {
        request_id: Hex32::new([81; 32]),
        digest: Hex32::new([82; 32]),
        expected: RecoveryExpected {
            work: RecoveryWork {
                agent: AgentId::from_parts(1, 81),
                goal: GoalId::from_parts(1, 82),
                turn: TurnId::from_parts(1, 83),
            },
            issue_lsn: Lsn(1),
            source_lsn: Lsn(1),
            episode: None,
            predecessor: None,
            control_lsn: None,
            ticket: None,
        },
        choice: RecoveryChoice::FreshWithRecap,
        attribution: RecoveryAttribution::LocalOperator {
            pid: 4242,
            uid: Some(501),
        },
    };
    let refusal = daemon.sup.recover(
        command.clone(),
        &dudes_supervisor::RecoveryOperation::empty(),
    );
    assert_eq!(
        refusal.recovery().unwrap().refusal,
        Some(RecoveryRefusal::StaleWork)
    );
    let mut report =
        PumpReport::Recovery(Box::new(CommandContext::new(operator_admission(), command)));
    report.recovery_update(&refusal).unwrap();
    let journal = fixture.entries();
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let queue = std::collections::VecDeque::from([Input::Persisted {
        token: *token,
        append,
    }]);
    daemon
        .pump_inputs(
            &mut report,
            PumpOrigin::Ordinary,
            queue,
            &[],
            |_, _operation| Ok(dudes_supervisor::input::Handled::default()),
        )
        .unwrap();
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(through));
    assert!(daemon.sup.world().agents[&hired.agent].paused);
    assert_eq!(fixture.entries(), journal);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(daemon.sup.effects().is_empty());
    assert!(!daemon.ctx.journal_writes_stopped());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

fn shared_instruction_completion(
    owner: dudes_proto::InstructionObservationOwner,
    ticket: dudes_proto::InstructionTicketAssociation,
    agent: dudes_types::AgentId,
    turn: dudes_types::TurnId,
) -> dudes_proto::Record {
    use dudes_proto::Record;
    use dudes_proto::instruction as I;
    use dudes_types::{Lsn, Timestamp};
    let change = I::InstructionPathChange {
        path: I::NativeInstructionPath::Utf8 {
            value: "/work/project/CLAUDE.md".into(),
        },
        context: I::InstructionPathContext::SharedProject,
        source: I::InstructionInputSource {
            agent,
            rule: 0,
            root: I::NativeInstructionPath::Utf8 {
                value: "/work/project".into(),
            },
            kind: I::InstructionSourceKind::Fixed,
        },
        before: I::InstructionFingerprint::Absent,
        after: I::InstructionFingerprint::File {
            digest: Hex32::new([2; 32]),
            bytes: 1,
        },
    };
    Record::InstructionObservationFinished {
        owner,
        ticket: Some(ticket),
        baseline: I::InstructionBaselineRef::Started { lsn: Lsn(4) },
        settlement: I::InstructionSettlement::Compared {
            end: I::InstructionEndBoundary::ManagedTurn {
                turn,
                end_lsn: Lsn(6),
            },
            sampled_at: Timestamp::UNIX_EPOCH,
            recovery: false,
            comparison: I::InstructionComparison {
                changes: vec![change],
                gaps: Vec::new(),
                usage: I::InstructionSampleUsage::default(),
            },
        },
    }
}

#[test]
fn instruction_wait_views_distinguish_finished_shared_review_from_pending_comparison() {
    use dudes_proto::instruction as I;
    use dudes_proto::record::samples;
    use dudes_proto::{Entry, Record, RecordKind};
    use dudes_types::{Lsn, Seq, Timestamp};
    let card = dudes_types::CardId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAZ").unwrap();
    let goal = dudes_types::GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap();
    let agent = dudes_types::AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let turn = dudes_types::TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    let hash = |bytes: &[u8]| {
        use sha2::{Digest as _, Sha256};
        Hex32::new(Sha256::digest(bytes).into())
    };
    let mut state = dudes_proto::instruction_state::InstructionState::default();
    for (lsn, kind) in [
        (1, RecordKind::TicketCreated),
        (2, RecordKind::TicketAssigned),
        (3, RecordKind::TicketStarted),
    ] {
        state
            .apply(
                Lsn(lsn),
                &Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: samples::record(kind),
                },
                None,
                hash,
            )
            .unwrap();
    }
    let ticket = I::InstructionTicketAssociation {
        card,
        goal,
        assignment_lsn: Lsn(2),
        seq_at_start: Seq(2),
    };
    let mut started = samples::record(RecordKind::InstructionObservationStarted);
    let Record::InstructionObservationStarted {
        owner,
        ticket: captured,
        ..
    } = &mut started
    else {
        panic!("started")
    };
    let owner = *owner;
    *captured = Some(ticket);
    state
        .apply(
            Lsn(4),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: started,
            },
            None,
            hash,
        )
        .unwrap();
    for (lsn, kind) in [(5, RecordKind::TurnIssued), (6, RecordKind::TurnEnded)] {
        state
            .apply(
                Lsn(lsn),
                &Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: samples::record(kind),
                },
                None,
                hash,
            )
            .unwrap();
    }
    assert!(state.pending_for_goal(goal));
    assert_eq!(
        instruction_waits(&state, Some(card)),
        (RecoveryWaitStatus::Ready, RecoveryWaitStatus::Waiting)
    );
    let finished = shared_instruction_completion(owner, ticket, agent, turn);
    state
        .apply(
            Lsn(7),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: finished,
            },
            None,
            hash,
        )
        .unwrap();
    assert!(state.pending_for_goal(goal));
    assert_eq!(
        instruction_waits(&state, Some(card)),
        (RecoveryWaitStatus::Waiting, RecoveryWaitStatus::Ready)
    );
    assert_eq!(
        instruction_waits(&state, None),
        (RecoveryWaitStatus::Ready, RecoveryWaitStatus::Ready)
    );
}
