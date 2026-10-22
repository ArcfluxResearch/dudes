use super::*;
use dudes_proto::rpc::ticket::{TicketReadParams, TicketReadResult};
use dudes_types::CardStatus;

fn reading(handle: &crate::Handle, card: dudes_types::CardId) -> TicketReadResult {
    let Answered::TicketRead(read) = handle
        .ask(crate::ports::Ask::Ticket(crate::ports::TicketAsk::Read(
            TicketReadParams { card },
        )))
        .unwrap()
    else {
        panic!("ticket read");
    };
    *read
}

fn browser_ticket_fixture(
    fixture: common::Fixture,
    mut daemon: TestDaemon,
    agent: AgentId,
    card: dudes_types::CardId,
    clock: Arc<common::TestClock>,
) -> BrowserFixture {
    let floor = daemon.floor.as_ref().unwrap();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let identity = sign_in(&sessions, &floor.addr().to_string());
    let inspected = daemon.recovery_inspect_browser(&identity, agent).unwrap();
    let RecoveryInspectResult::Available { expected, .. } = inspected else {
        panic!("authenticated browser inspection: {inspected:?}");
    };
    assert_eq!(expected.ticket.unwrap().card, card);
    assert_eq!(expected.work.agent, agent);
    BrowserFixture {
        fixture,
        daemon,
        sessions,
        identity,
        intent: RecoverySubmitParams {
            request_id: Hex32::new([121; 32]),
            expected: *expected,
            choice: WireChoice::FreshWithRecap,
        },
        clock,
    }
}

#[test]
fn recovery_owned_status_changes_preserve_a_real_waiting_browser_claim() {
    let fixture = common::Fixture::new();
    let mut settings = floor_settings(&fixture, 0);
    settings.floor_answer_enabled = true;
    settings.floor_board_enabled = true;
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let command = recoverable_ticket(&mut daemon, &fixture);
    let agent = command.expected.work.agent;
    let card = command.expected.ticket.unwrap().0;
    let required = daemon.sup.recovery_opportunity(agent).unwrap().required;
    append_history(&mut daemon, Record::RecoveryRequired(required));
    daemon.ticket_reactions().unwrap();
    let before = daemon.ctx.projections.ticket(card).unwrap().unwrap();
    assert_eq!(before.0.status, CardStatus::Blocked);
    assert_eq!(
        before.1.reason,
        Some(dudes_proto::TicketReason::RecoveryRequired)
    );
    let question_lsn = daemon.ctx.writer.next_lsn().unwrap();
    wait_for_human(&mut daemon, agent);
    let mut owned = browser_ticket_fixture(fixture, daemon, agent, card, clock);
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    assert!(
        matches!(submitted, RecoverySubmitResult::Accepted { ref receipt } if receipt.status == RecoveryReceiptState::Pending),
        "{submitted:?}"
    );
    owned.daemon.ticket_reactions().unwrap();
    let pending = owned.daemon.ctx.projections.ticket(card).unwrap().unwrap();
    assert_eq!(pending.0.status, CardStatus::Blocked);
    assert_eq!(
        pending.1.reason,
        Some(dudes_proto::TicketReason::RecoveryRequired)
    );
    assert!(
        matches!(owned.daemon.ctx.recovery_read(owned.intent.request_id), RecoveryReceiptResult::Found { ref receipt } if receipt.status == RecoveryReceiptState::Pending)
    );
    let request = crate::floor_answer::AnswerRequest::parse(&serde_json::to_vec(&serde_json::json!({
        "request_id":"BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "agent":agent, "question_lsn":question_lsn.0.to_string(), "text":"Continue the same ticket after this answer."
    })).unwrap()).unwrap();
    let host = owned.daemon.ctx.host.clone();
    let mut running = common::start(owned.daemon);
    assert!(matches!(
        running.handle.answer(owned.identity, request).unwrap(),
        crate::floor_answer::AnswerOutcome::Answered { .. }
    ));
    let admission =
        owned
            .fixture
            .await_record(
                "a continued issue or its existing refusal gate",
                |record| match record {
                    Record::TurnIssued { turn, .. } if *turn != command.expected.work.turn => {
                        Some(record.clone())
                    }
                    Record::AgentHeld { .. } => Some(record.clone()),
                    _ => None,
                },
            );
    assert!(
        matches!(admission, Record::TurnIssued { .. }),
        "admission={admission:?}"
    );
    let consumed = await_consumed(&owned.fixture, &running.handle, owned.intent.request_id);
    assert!(matches!(
        consumed.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    let after = reading(&running.handle, card);
    assert_eq!(after.card.status, CardStatus::Doing);
    assert_eq!(
        (after.card.claimed_by, after.ticket.goal, after.ticket.round),
        (before.0.claimed_by, before.1.goal, before.1.round)
    );
    assert_eq!(host.spawned().len(), 1);
    let entries = owned.fixture.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record, Record::RecoveryAuthorized(_)))
            .count(),
        1
    );
    assert!(
        !entries
            .iter()
            .any(|(_, entry)| matches!(entry.record, Record::RecoveryInvalidated(_)))
    );
    running.stop(crate::ExitCause::Ended);
}
