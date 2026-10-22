use dudes_journal::{JournalOptions, Writer};
use dudes_proto::instruction_state::{InstructionReviewFacts, InstructionState};
use dudes_proto::record::{Hex32, samples};
use dudes_proto::rpc::ticket::{
    TicketAcknowledgedReview, TicketAcknowledgmentAction, TicketPriorReview, TicketRecoveredReview,
};
use dudes_proto::{
    Actor, Entry, InstructionBaselineRef, InstructionComparison, InstructionCoverageGap,
    InstructionGapKind, InstructionGapOrigin, InstructionIssueAssociation,
    InstructionObservationOwner, InstructionPathContext, InstructionReviewAction,
    InstructionSettlement, InstructionTicketAssociation, InstructionUnknownReason, Record,
    RecordKind,
};
use dudes_types::{AgentId, CardId, Lsn, Timestamp, TurnId};

use super::super::acknowledgment;

fn hash(bytes: &[u8]) -> Hex32 {
    Hex32::new(*blake3::hash(bytes).as_bytes())
}

fn append(
    writer: &mut Writer,
    state: &mut InstructionState,
    record: Record,
    facts: Option<InstructionReviewFacts>,
) -> Lsn {
    let entry = Entry {
        at: Timestamp::from_unix_millis(123),
        record,
    };
    state.validate_next(&entry, facts, hash).unwrap();
    let lsn = writer.append(&entry).unwrap();
    state.apply(lsn, &entry, facts, hash).unwrap();
    lsn
}

fn unknown(
    owner: InstructionObservationOwner,
    ticket: InstructionTicketAssociation,
    started: Lsn,
) -> Record {
    let InstructionObservationOwner::Managed { agent, .. } = owner else {
        unreachable!()
    };
    Record::InstructionObservationFinished {
        owner,
        ticket: Some(ticket),
        baseline: InstructionBaselineRef::Started { lsn: started },
        settlement: InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
            end: None,
            sampled_at: None,
            recovery: true,
            comparison: InstructionComparison {
                changes: Vec::new(),
                gaps: vec![InstructionCoverageGap {
                    origin: InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::UnrecordedLaunchBoundary,
                    context: InstructionPathContext::SharedProject,
                    agent: Some(agent),
                    path: None,
                }],
                usage: dudes_proto::InstructionSampleUsage::default(),
            },
        },
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    writer: Writer,
    state: InstructionState,
    card: CardId,
    baselines: [Lsn; 2],
    expected: TicketAcknowledgedReview,
}

fn two_intervals(
    writer: &mut Writer,
    state: &mut InstructionState,
    agent: AgentId,
    ticket: InstructionTicketAssociation,
) -> [Lsn; 2] {
    let mut baselines = [Lsn::ZERO; 2];
    for (index, started) in baselines.iter_mut().enumerate() {
        let owner = InstructionObservationOwner::Managed {
            agent,
            turn: TurnId::from_parts(30, u128::try_from(index).unwrap()),
            issue: InstructionIssueAssociation::Prospective,
        };
        let Record::InstructionObservationStarted { snapshot, .. } =
            samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!()
        };
        *started = append(
            writer,
            state,
            Record::InstructionObservationStarted {
                owner,
                ticket: Some(ticket),
                snapshot,
            },
            None,
        );
        append(writer, state, unknown(owner, ticket, *started), None);
    }
    baselines
}

fn record_ack(
    writer: &mut Writer,
    state: &mut InstructionState,
    agent: AgentId,
    ticket: InstructionTicketAssociation,
    baselines: [Lsn; 2],
) -> TicketAcknowledgedReview {
    let watermark = state.watermark(ticket.card, hash).unwrap().unwrap();
    assert_eq!(
        state
            .review(ticket.card, hash)
            .unwrap()
            .unwrap()
            .intervals
            .len(),
        2
    );
    let expected_seq = ticket.seq_at_start;
    let ack = append(
        writer,
        state,
        Record::TicketInstructionReviewed {
            card: ticket.card,
            by: Actor::Peer {
                pid: 77,
                uid: Some(501),
            },
            expected_seq,
            watermark,
            action: InstructionReviewAction::ChangesRequested {
                agent,
                message: Some("private-path-or-transcript".into()),
                clear_goal: None,
            },
        },
        Some(InstructionReviewFacts {
            card: ticket.card,
            seq: expected_seq,
            goal: Some(ticket.goal),
            completed_lsn: None,
            clear_goal: None,
        }),
    );
    TicketAcknowledgedReview {
        lsn: ack,
        expected_seq,
        watermark,
        at: Timestamp::from_unix_millis(123),
        action: TicketAcknowledgmentAction::ChangesRequested,
        intervals: baselines
            .map(|lsn| InstructionBaselineRef::Started { lsn })
            .to_vec(),
    }
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let (mut writer, _) = dudes_journal::open(directory.path(), JournalOptions::default()).unwrap();
    let mut state = InstructionState::default();
    for kind in [
        RecordKind::TicketCreated,
        RecordKind::TicketAssigned,
        RecordKind::TicketStarted,
    ] {
        append(&mut writer, &mut state, samples::record(kind), None);
    }
    let Record::TicketStarted {
        agent,
        goal,
        expected_seq,
        ..
    } = samples::record(RecordKind::TicketStarted)
    else {
        unreachable!()
    };
    let ticket = state
        .association_for_goal(goal.id, expected_seq.next())
        .unwrap();
    let baselines = two_intervals(&mut writer, &mut state, agent, ticket);
    let expected = record_ack(&mut writer, &mut state, agent, ticket, baselines);
    Fixture {
        _directory: directory,
        writer,
        state,
        card: ticket.card,
        baselines,
        expected,
    }
}

#[test]
fn one_actual_acknowledgment_groups_distinct_validated_intervals_with_full_metadata() {
    let fixture = fixture();
    let reader = fixture.writer.reader();
    let mut view = TicketRecoveredReview::default();
    for started in fixture.baselines {
        let attempt = fixture.state.attempt(started).unwrap();
        assert_eq!(attempt.reviewed_lsn, Some(fixture.expected.lsn));
        assert_eq!(
            acknowledgment(
                &mut view,
                attempt,
                fixture.card,
                &reader,
                fixture.expected.lsn
            ),
            TicketPriorReview::Found {
                lsn: fixture.expected.lsn
            }
        );
    }
    assert_eq!(view.acknowledgments, vec![fixture.expected.clone()]);
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("private-path-or-transcript")
    );
}
