//! A ticket is a board card with more to remember, and its start installs a goal. These tests drive
//! the projections with ticket records and compare every outcome with what the same story
//! produces for a plain card or a `GoalSet`.

use dudes_journal::{Fsync, JournalOptions, Projections, Reader, open, replay};
use dudes_proto::record::samples;
use dudes_proto::{
    ActivityEvent, Actor, Entry, Goal, QueueClass, QueueKey, Record, RecordKind, TicketCheck,
    TicketReason,
};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Lsn, Seq, Timestamp};

fn operator() -> Actor {
    Actor::Peer { pid: 41, uid: None }
}

fn card() -> CardId {
    CardId::from_parts(4, 1)
}

fn agent() -> AgentId {
    AgentId::from_parts(1, 1)
}

fn hire(workspace: &str) -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id, workspace: at, ..
    } = &mut hired
    else {
        unreachable!("the sample is a hire");
    };
    *id = agent();
    *at = Some(dudes_proto::record::RecordedWorkspace {
        id: workspace.into(),
        label: "Service".into(),
    });
    hired
}

fn check() -> TicketCheck {
    TicketCheck {
        id: "default".into(),
        label: "tests pass".into(),
        predicate: dudes_proto::Predicate::Command {
            argv: vec!["cargo".into(), "test".into()],
            timeout_s: 60,
        },
    }
}

fn created(card: CardId) -> Record {
    Record::TicketCreated {
        card,
        title: "Fix the parser".into(),
        description: "Empty input panics.".into(),
        department: "engineering".into(),
        project: "service".into(),
        done_when: check(),
        by: operator(),
    }
}

fn assigned(card: CardId, expected_seq: u64, message: Option<&str>) -> Record {
    Record::TicketAssigned {
        card,
        agent: agent(),
        by: operator(),
        message: message.map(str::to_owned),
        expected_seq: Seq(expected_seq),
    }
}

fn goal(id: GoalId) -> Goal {
    let Record::GoalSet { goal, .. } = samples::record(RecordKind::GoalSet) else {
        unreachable!("the sample is a goal set");
    };
    Goal {
        id,
        text: "Fix the parser".into(),
        predicate: check().predicate,
        ..goal
    }
}

fn started(card: CardId, goal_id: GoalId, expected_seq: u64) -> Record {
    Record::TicketStarted {
        card,
        agent: agent(),
        goal: goal(goal_id),
        expected_seq: Seq(expected_seq),
    }
}

fn transition(card: CardId, from: CardStatus, to: CardStatus, expected_seq: u64) -> Record {
    Record::CardTransition {
        card,
        from,
        to,
        by: Actor::System,
        expected_seq: Seq(expected_seq),
        reason: None,
    }
}

fn project(records: impl IntoIterator<Item = Record>) -> Projections {
    let mut projections = Projections::in_memory().unwrap();
    for (index, record) in records.into_iter().enumerate() {
        let entry = Entry {
            at: Timestamp::from_unix_millis(1_000 + i64::try_from(index).unwrap()),
            record,
        };
        projections
            .apply(
                Lsn(u64::try_from(index).unwrap() + 1),
                u32::try_from(index).unwrap(),
                &entry,
            )
            .unwrap();
    }
    projections
}

#[test]
fn a_created_ticket_is_a_todo_card_with_its_note_project_and_check() {
    let projections = project([created(card())]);
    let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Todo);
    assert_eq!(view.status_by, operator());
    assert_eq!(view.seq, Seq::ZERO);
    assert_eq!(view.claimed_by, None);
    assert_eq!(view.title, "Fix the parser");
    assert_eq!(ticket.description, "Empty input panics.");
    assert_eq!(ticket.project, "service");
    assert_eq!(ticket.done_when, check());
    assert_eq!((ticket.round, ticket.queue, ticket.goal), (0, None, None));
    assert_eq!(projections.cards().unwrap(), vec![view]);
}

#[test]
fn a_plain_card_is_not_a_ticket() {
    let plain = CardId::from_parts(4, 2);
    let projections = project([samples_card(plain), created(card())]);
    assert!(projections.ticket(plain).unwrap().is_none());
    assert!(projections.card(plain).unwrap().is_some());
    assert_eq!(projections.tickets().unwrap().len(), 1);
}

fn samples_card(card: CardId) -> Record {
    Record::CardCreated {
        card,
        title: "Plain".into(),
        department: "engineering".into(),
        by: operator(),
        deps: Vec::new(),
    }
}

/// An edit replaces the three values and moves the sequence, and leaves the status, the holder and
/// the queue place alone.
#[test]
fn an_edit_changes_the_words_and_keeps_the_queue_place() {
    let projections = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, None),
        Record::TicketEdited {
            card: card(),
            title: "Fix the lexer".into(),
            description: String::new(),
            done_when: TicketCheck {
                id: "lint".into(),
                label: "lint is clean".into(),
                ..check()
            },
            by: operator(),
            expected_seq: Seq(1),
        },
    ]);
    let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
    assert_eq!(view.title, "Fix the lexer");
    assert_eq!(view.status, CardStatus::Claimed);
    assert_eq!(view.claimed_by, Some(agent()));
    assert_eq!(view.seq, Seq(2));
    assert_eq!(ticket.description, "");
    assert_eq!(ticket.done_when.id, "lint");
    assert_eq!(
        ticket.queue,
        Some(QueueKey {
            class: QueueClass::New,
            lsn: Lsn(3),
        })
    );
}

#[test]
fn an_assignment_queues_the_ticket_and_keeps_the_message_for_the_next_start() {
    let projections = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, Some("Please cover the empty input.")),
    ]);
    let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Claimed);
    assert_eq!(view.status_by, operator());
    assert_eq!(view.claimed_by, Some(agent()));
    assert_eq!(
        ticket.message.as_deref(),
        Some("Please cover the empty input.")
    );
    assert_eq!(ticket.queue.unwrap().class, QueueClass::New);
}

/// Work given again from Ready for you or Needs you is served before work given for the first
/// time, whatever the order of the assignments.
#[test]
fn a_ticket_given_again_is_rework() {
    for from in [CardStatus::Review, CardStatus::Blocked] {
        let projections = project([
            hire("service"),
            created(card()),
            assigned(card(), 0, None),
            started(card(), GoalId::from_parts(8, 1), 1),
            transition(card(), CardStatus::Doing, from, 2),
            assigned(card(), 3, Some("Again.")),
        ]);
        let (_, ticket) = projections.ticket(card()).unwrap().unwrap();
        assert_eq!(ticket.queue.unwrap().class, QueueClass::Rework, "{from:?}");
        assert_eq!(ticket.queue.unwrap().lsn, Lsn(6));
    }
}

/// A ticket that is already queued as rework keeps its class and its sent-back message when it is
/// moved to the same agent or another, and a new message replaces the old one.
#[test]
fn moving_a_queued_rework_ticket_keeps_its_class_and_message() {
    let other = AgentId::from_parts(1, 2);
    let mut records = vec![
        hire("service"),
        created(card()),
        assigned(card(), 0, None),
        started(card(), GoalId::from_parts(8, 1), 1),
        transition(card(), CardStatus::Doing, CardStatus::Review, 2),
        assigned(card(), 3, Some("Cover the edge.")),
    ];
    let queue = |records: &[Record]| {
        let projections = project(records.iter().cloned());
        let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
        (view.claimed_by, ticket.queue.unwrap(), ticket.message)
    };
    let (_, first, message) = queue(&records);
    assert_eq!(first.class, QueueClass::Rework);
    assert_eq!(message.as_deref(), Some("Cover the edge."));

    records.push(assigned(card(), 4, None));
    let (_, again, message) = queue(&records);
    assert_eq!(again.class, QueueClass::Rework, "still ahead of new work");
    assert!(again.lsn > first.lsn, "but moved to the back of the rework");
    assert_eq!(
        message.as_deref(),
        Some("Cover the edge."),
        "no message keeps it"
    );

    records.push(Record::TicketAssigned {
        card: card(),
        agent: other,
        by: operator(),
        message: Some("Cover the edge and the end.".into()),
        expected_seq: Seq(5),
    });
    let (holder, moved, message) = queue(&records);
    assert_eq!(holder, Some(other));
    assert_eq!(moved.class, QueueClass::Rework);
    assert_eq!(message.as_deref(), Some("Cover the edge and the end."));
}

/// A ticket record that names a plain card changes nothing on it.
#[test]
fn ticket_records_leave_a_plain_card_alone() {
    let plain = CardId::from_parts(4, 2);
    let base = [hire("service"), samples_card(plain)];
    let untouched = project(base.iter().cloned()).card(plain).unwrap().unwrap();
    for record in [
        Record::TicketEdited {
            card: plain,
            title: "Hijacked".into(),
            description: String::new(),
            done_when: check(),
            by: operator(),
            expected_seq: Seq::ZERO,
        },
        assigned(plain, 0, Some("hi")),
        started(plain, GoalId::from_parts(8, 1), 0),
        Record::TicketRemoved {
            card: plain,
            by: operator(),
            expected_seq: Seq::ZERO,
        },
    ] {
        let kind = record.kind();
        let projections = project(base.iter().cloned().chain([record]));
        assert_eq!(
            projections.card(plain).unwrap().as_ref(),
            Some(&untouched),
            "{kind:?}"
        );
    }
}

#[test]
fn a_start_moves_the_card_to_doing_and_links_its_goal() {
    let goal_id = GoalId::from_parts(8, 1);
    let projections = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, Some("Mind the empty input.")),
        started(card(), goal_id, 1),
    ]);
    let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Doing);
    assert_eq!(view.status_by, Actor::System);
    assert_eq!(view.seq, Seq(2));
    assert_eq!(view.claimed_by, Some(agent()));
    assert_eq!(ticket.goal, Some(goal_id));
    assert_eq!(ticket.round, 1);
    assert_eq!(ticket.queue, None);
    assert_eq!(ticket.message, None, "the message was spent by the start");
}

#[test]
fn a_removed_ticket_leaves_the_board() {
    let projections = project([
        created(card()),
        Record::TicketRemoved {
            card: card(),
            by: operator(),
            expected_seq: Seq::ZERO,
        },
    ]);
    assert!(projections.card(card()).unwrap().is_none());
    assert!(projections.ticket(card()).unwrap().is_none());
    assert!(projections.cards().unwrap().is_empty());
}

#[test]
fn a_needs_you_move_keeps_its_reason_until_the_ticket_moves_again() {
    let mut blocked = transition(card(), CardStatus::Doing, CardStatus::Blocked, 2);
    let Record::CardTransition { reason, .. } = &mut blocked else {
        unreachable!("built as a transition");
    };
    *reason = Some(TicketReason::NoTurns);
    let mut projections = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, None),
        started(card(), GoalId::from_parts(8, 1), 1),
        blocked,
    ]);
    assert_eq!(
        projections.ticket(card()).unwrap().unwrap().1.reason,
        Some(TicketReason::NoTurns)
    );
    let entry = Entry {
        at: Timestamp::from_unix_millis(9_000),
        record: transition(card(), CardStatus::Blocked, CardStatus::Review, 3),
    };
    projections.apply(Lsn(10), 10, &entry).unwrap();
    assert_eq!(projections.ticket(card()).unwrap().unwrap().1.reason, None);
}

/// Stop's move into To do drops the holder and the queue place of a ticket, and a ticket given to
/// an agent no longer lists it among its queue.
#[test]
fn a_ticket_moved_to_todo_loses_its_holder() {
    let projections = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, None),
        started(card(), GoalId::from_parts(8, 1), 1),
        transition(card(), CardStatus::Doing, CardStatus::Todo, 2),
    ]);
    let (view, ticket) = projections.ticket(card()).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Todo);
    assert_eq!(view.claimed_by, None);
    assert_eq!(ticket.queue, None);
    assert_eq!(
        ticket.goal,
        Some(GoalId::from_parts(8, 1)),
        "the link is history"
    );
}

fn retired() -> Record {
    Record::AgentRetired {
        id: agent(),
        reason: "left".into(),
        browser_retire: None,
    }
}

/// Retiring an agent returns the work it was doing to the list, and leaves a pending decision
/// where it is: a ticket in Ready for you stays there, with the agent that did the work.
#[test]
fn retiring_an_agent_leaves_ready_for_you_and_returns_the_rest() {
    let story = |status: Option<CardStatus>| {
        let mut records = vec![hire("service"), created(card()), assigned(card(), 0, None)];
        match status {
            Some(CardStatus::Claimed) | None => {}
            Some(to) => {
                records.push(started(card(), GoalId::from_parts(8, 1), 1));
                if to != CardStatus::Doing {
                    records.push(transition(card(), CardStatus::Doing, to, 2));
                }
            }
        }
        records.push(retired());
        project(records).ticket(card()).unwrap().unwrap()
    };

    let (view, ticket) = story(Some(CardStatus::Review));
    assert_eq!(
        view.status,
        CardStatus::Review,
        "the decision is not erased"
    );
    assert_eq!(view.claimed_by, Some(agent()), "the assignee is history");
    assert_eq!(view.seq, Seq(3), "nothing moved the card");
    assert_eq!(ticket.goal, Some(GoalId::from_parts(8, 1)));

    for status in [
        Some(CardStatus::Doing),
        Some(CardStatus::Blocked),
        Some(CardStatus::Claimed),
    ] {
        let (view, ticket) = story(status);
        assert_eq!(view.status, CardStatus::Todo, "{status:?}");
        assert_eq!(view.status_by, Actor::System, "{status:?}");
        assert_eq!(view.claimed_by, None, "{status:?}");
        assert_eq!(ticket.queue, None, "{status:?}");
    }
}

/// The control: a plain card in review still goes back to the list when its agent retires.
#[test]
fn a_plain_card_in_review_still_returns_to_todo_on_retirement() {
    let plain = CardId::from_parts(4, 2);
    let projections = project([
        hire("service"),
        samples_card(plain),
        Record::Claim {
            card: plain,
            agent: agent(),
            expected_seq: Seq::ZERO,
        },
        transition(plain, CardStatus::Claimed, CardStatus::Review, 1),
        retired(),
    ]);
    let view = projections.card(plain).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Todo);
    assert_eq!(view.claimed_by, None);
}

/// A ticket start reaches the goals row, the agent row and the activity feed exactly as a
/// `GoalSet` for the same goal does.
#[test]
fn goals_row_for_a_ticket_goal() {
    let goal_id = GoalId::from_parts(8, 1);
    let ticket = project([hire("service"), started(card(), goal_id, 0)]);
    let plain = project([
        hire("service"),
        Record::GoalSet {
            agent: agent(),
            goal: goal(goal_id),
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    ]);
    let row = ticket.goal(goal_id).unwrap().expect("the goal has a row");
    assert_eq!(row.agent, agent());
    assert_eq!(row.text, "Fix the parser");
    assert_eq!(row.predicate, check().predicate);
    assert_eq!(
        serde_json::to_value(&row).unwrap(),
        serde_json::to_value(plain.goal(goal_id).unwrap().unwrap()).unwrap()
    );
}

#[test]
fn agent_row_takes_a_ticket_goal() {
    let goal_id = GoalId::from_parts(8, 1);
    let projections = project([hire("service"), started(card(), goal_id, 0)]);
    let row = projections.agent(agent()).unwrap().unwrap();
    assert_eq!(row.goal, Some(goal_id));
    assert_eq!(row.context_lsn, Lsn(2));
    assert!(row.automatic_hold.is_none());
}

#[test]
fn activity_row_for_ticket_start() {
    let projections = project([
        hire("service"),
        started(card(), GoalId::from_parts(8, 1), 0),
    ]);
    let activity = projections.activity().unwrap();
    assert!(
        matches!(
            activity.last().map(|row| &row.event),
            Some(ActivityEvent::GoalSet { agent: shown }) if shown.id == agent()
        ),
        "{activity:?}"
    );
}

fn write_story(dir: &std::path::Path, checkpoint_every: u64) -> Reader {
    let (mut writer, _) = open(
        dir,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let records = [
        hire("service"),
        created(card()),
        assigned(card(), 0, Some("Mind the empty input.")),
        started(card(), GoalId::from_parts(8, 1), 1),
        transition(card(), CardStatus::Doing, CardStatus::Review, 2),
        retired(),
    ];
    for (index, record) in records.into_iter().enumerate() {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_000 + i64::try_from(index).unwrap()),
                record,
            })
            .unwrap();
    }
    drop(writer);
    Reader::open(dir).unwrap().0
}

/// The journal alone rebuilds the board: a projection folded live and one replayed from the
/// frames agree on every ticket field, with checkpoints interleaved and without.
#[test]
fn replay_rebuilds_the_tickets_with_and_without_checkpoints() {
    let mut rows = Vec::new();
    for checkpoint_every in [0, 2] {
        let dir = tempfile::tempdir().unwrap();
        let reader = write_story(dir.path(), checkpoint_every);
        let mut replayed = Projections::in_memory().unwrap();
        replay(&reader, &mut replayed).unwrap();
        rows.push(replayed.tickets().unwrap());
    }
    assert_eq!(rows[0], rows[1]);
    let (view, ticket) = &rows[0][0];
    assert_eq!(view.status, CardStatus::Review);
    assert_eq!(ticket.round, 1);
    let live = project([
        hire("service"),
        created(card()),
        assigned(card(), 0, Some("Mind the empty input.")),
        started(card(), GoalId::from_parts(8, 1), 1),
        transition(card(), CardStatus::Doing, CardStatus::Review, 2),
        retired(),
    ]);
    assert_eq!(rows[0], live.tickets().unwrap());
}
