use super::*;
use crate::runtime::Witness;
use crate::runtime::work_test_support as common;
use std::sync::Arc;

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
        .handle(Input::Command(Command::Pause {
            agent: hired.agent,
            browser: None,
        }))
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
    let refusal = daemon.sup.recover(command.clone());
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
        .pump_inputs(&mut report, PumpOrigin::Ordinary, queue, |_| {
            Ok(dudes_supervisor::input::Handled::default())
        })
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
