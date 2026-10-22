use super::*;
use dudes_proto::record::samples;
use dudes_types::Timestamp;

mod grouping;

fn attempt(reviewed: Lsn) -> InstructionAttempt {
    let Record::InstructionObservationStarted { owner, .. } =
        samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    InstructionAttempt {
        baseline: InstructionBaselineRef::Started { lsn: Lsn(10) },
        owner,
        ticket: None,
        snapshot: None,
        finished: None,
        reviewed_lsn: Some(reviewed),
        spawn_lsn: None,
        boundary_settled: None,
    }
}

#[test]
fn historical_acknowledgment_requires_the_exact_card_kind_and_frame() {
    let directory = tempfile::tempdir().unwrap();
    let (mut writer, _) =
        dudes_journal::open(directory.path(), dudes_journal::JournalOptions::default()).unwrap();
    let Record::TicketInstructionReviewed { card, .. } =
        samples::record(RecordKind::TicketInstructionReviewed)
    else {
        unreachable!();
    };
    let mut record = samples::record(RecordKind::TicketInstructionReviewed);
    if let Record::TicketInstructionReviewed { action, .. } = &mut record {
        *action = InstructionReviewAction::ChangesRequested {
            agent: dudes_types::AgentId::from_parts(1, 1),
            message: Some("private-path-or-transcript".into()),
            clear_goal: None,
        };
    }
    let ack = writer
        .append(&Entry {
            at: Timestamp::from_unix_millis(123),
            record,
        })
        .unwrap();
    let other = writer
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: samples::record(RecordKind::AgentHired),
        })
        .unwrap();
    let reader = writer.reader();
    let mut view = TicketRecoveredReview::default();
    assert_eq!(
        acknowledgment(&mut view, &attempt(ack), card, &reader, other),
        TicketPriorReview::Found { lsn: ack }
    );
    assert_eq!(
        view.acknowledgments[0].action,
        TicketAcknowledgmentAction::ChangesRequested
    );
    assert_eq!(view.acknowledgments[0].at, Timestamp::from_unix_millis(123));
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("private-path-or-transcript")
    );
    for (source, target) in [
        (other, card),
        (Lsn(u64::MAX), card),
        (ack, dudes_types::CardId::from_parts(99, 9)),
    ] {
        let mut view = TicketRecoveredReview::default();
        assert_eq!(
            acknowledgment(&mut view, &attempt(source), target, &reader, other),
            TicketPriorReview::Unavailable
        );
        assert!(view.acknowledgments.is_empty());
    }
}

#[test]
fn no_host_call_is_distinct_from_native_settlement_and_other_attempt_effects() {
    let mut interval = attempt(Lsn(31));
    interval.finished = Some(dudes_proto::instruction_state::InstructionFinished {
        lsn: Lsn(30),
        settlement: InstructionSettlement::NoProcess {
            proof_lsn: Lsn(29),
            proof: dudes_proto::InstructionNoProcessProof::NoHostCall,
        },
    });
    assert_eq!(
        prior_boundary(Some(&interval)),
        TicketPriorBoundary::NoHostCall { proof_lsn: Lsn(29) }
    );
    interval.finished = None;
    assert_eq!(
        prior_boundary(Some(&interval)),
        TicketPriorBoundary::Unsettled
    );
    assert_eq!(prior_boundary(None), TicketPriorBoundary::Unavailable);
}
