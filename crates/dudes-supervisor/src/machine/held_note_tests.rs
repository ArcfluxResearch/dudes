use super::*;
use crate::testing::Harness;
use dudes_proto::goal::Predicate;
use dudes_proto::instruction::{InstructionAcceptCompletion, InstructionReviewAction};
use dudes_proto::{
    BrowserNoteAttribution, BrowserNoteDestination, ExitStatus, GoalBudget, RecordKind,
};
use dudes_types::{CardId, Seq, Usd};

fn card() -> CardId {
    CardId::from_parts(5, 1)
}

fn issued_turns(harness: &Harness) -> usize {
    harness
        .records()
        .filter(|record| record.kind() == RecordKind::TurnIssued || record.turn_issue().is_some())
        .count()
}

fn spawns(harness: &Harness) -> usize {
    harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, crate::testing::Seen::Spawn { .. }))
        .count()
}

/// A ticket round whose passing check ended the goal while the round's instruction review is
/// still pending.
fn round_waiting_on_review() -> (Harness, AgentId, GoalId) {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.board_changed(Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::TicketAssigned {
            card: card(),
            agent,
            by: dudes_proto::Actor::Peer { pid: 17, uid: None },
            message: None,
            expected_seq: Seq(0),
            priority: None,
        },
    });
    harness.drive(Input::Command(Command::TicketStart {
        card: card(),
        agent,
        text: "ship it".into(),
        predicate: Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 600,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 8,
        completion_check: None,
        expected_seq: Seq(1),
        repositories: None,
    }));
    let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
    let turn = harness.open_turn(agent).expect("a turn is running");
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
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
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: dudes_proto::Evidence::Command {
            argv: vec!["true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    (harness, agent, goal)
}

fn review_accepted(harness: &Harness) -> Entry {
    let mut instructions = dudes_proto::instruction_state::InstructionState::default();
    let hash = |bytes: &[u8]| dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes());
    for (index, entry) in harness.journal.iter().enumerate() {
        instructions
            .apply_validated(Lsn(u64::try_from(index).unwrap() + 1), entry, hash)
            .unwrap();
    }
    let watermark = instructions
        .watermark(card(), hash)
        .unwrap()
        .expect("the round needs a review");
    Entry {
        at: harness.journal.last().unwrap().at,
        record: Record::TicketInstructionReviewed {
            card: card(),
            by: dudes_proto::Actor::Peer { pid: 17, uid: None },
            expected_seq: Seq(2),
            watermark,
            action: InstructionReviewAction::Accept {
                completion: InstructionAcceptCompletion::Continue,
            },
        },
    }
}

#[test]
fn a_note_decided_during_the_review_is_issued_once_the_review_is_recorded() {
    let (mut harness, agent, goal) = round_waiting_on_review();
    assert!(
        harness
            .supervisor
            .world()
            .instructions
            .pending_for_goal(goal),
        "the round is waiting on its review"
    );
    let issued_before = issued_turns(&harness);
    harness.drive(Input::Command(Command::BrowserNoteQueued {
        agent,
        text: "hello after the review".into(),
        browser: BrowserNoteAttribution {
            request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
            request_digest: dudes_proto::record::Hex32::new([2; 32]),
            session_digest: dudes_proto::record::Hex32::new([3; 32]),
        },
        destination: BrowserNoteDestination::ManagedGoal { goal },
    }));
    assert_eq!(
        harness
            .records()
            .filter(|record| record.kind() == RecordKind::IdentifiedTaughtNoteTurnDecided)
            .count(),
        1,
        "the note was decided while the review was pending"
    );
    assert_eq!(issued_turns(&harness), issued_before);
    let spawned_before = spawns(&harness);

    let reviewed = review_accepted(&harness);
    harness.board_changed(reviewed);

    assert_eq!(issued_turns(&harness), issued_before + 1);
    assert_eq!(spawns(&harness), spawned_before + 1);
}

#[test]
fn a_review_with_no_waiting_note_issues_nothing() {
    let (mut harness, _, _) = round_waiting_on_review();
    let issued_before = issued_turns(&harness);
    let reviewed = review_accepted(&harness);
    harness.board_changed(reviewed);
    assert_eq!(issued_turns(&harness), issued_before);
}
