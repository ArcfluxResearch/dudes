//! A ticket's priority orders its project's queue, and its history records what happened to it.
//! The journals here are replayed through the projections the way a restart replays them.

use dudes_journal::Projections;
use dudes_proto::record::samples;
use dudes_proto::{
    Actor, Entry, Goal, HistoryBy, Priority, QueueClass, Record, RecordKind, TicketCheck,
    TicketHistoryKind, TicketReason,
};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Lsn, Seq, Timestamp};

const LEGACY: &str = include_str!("fixtures/ticket_queue_without_priority.jsonl");

fn operator() -> Actor {
    Actor::Peer { pid: 41, uid: None }
}

fn agent() -> AgentId {
    AgentId::from_parts(1, 1)
}

fn card(n: u128) -> CardId {
    CardId::from_parts(4, n)
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

fn created(n: u128, priority: Option<Priority>) -> Record {
    Record::TicketCreated {
        card: card(n),
        title: format!("c{n}"),
        description: String::new(),
        department: "engineering".into(),
        project: "service".into(),
        done_when: check(),
        by: operator(),
        request: None,
        priority,
    }
}

fn assigned(n: u128, expected_seq: u64, priority: Option<Priority>) -> Record {
    Record::TicketAssigned {
        card: card(n),
        agent: agent(),
        by: operator(),
        message: None,
        expected_seq: Seq(expected_seq),
        priority,
    }
}

fn edited(n: u128, expected_seq: u64, title: &str, priority: Option<Priority>) -> Record {
    Record::TicketEdited {
        card: card(n),
        title: title.into(),
        description: String::new(),
        done_when: check(),
        by: operator(),
        expected_seq: Seq(expected_seq),
        priority,
    }
}

fn goal(id: GoalId) -> Goal {
    let Record::GoalSet { goal, .. } = samples::record(RecordKind::GoalSet) else {
        unreachable!("the sample is a goal set");
    };
    Goal {
        id,
        text: "work".into(),
        predicate: check().predicate,
        ..goal
    }
}

fn started(n: u128, goal_id: GoalId, expected_seq: u64) -> Record {
    Record::TicketStarted {
        card: card(n),
        agent: agent(),
        goal: goal(goal_id),
        expected_seq: Seq(expected_seq),
    }
}

fn moved(
    n: u128,
    from: CardStatus,
    to: CardStatus,
    expected_seq: u64,
    reason: Option<TicketReason>,
) -> Record {
    Record::CardTransition {
        card: card(n),
        from,
        to,
        by: Actor::System,
        expected_seq: Seq(expected_seq),
        reason,
    }
}

fn entry(index: usize, record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_000 + i64::try_from(index).unwrap()),
        record,
    }
}

fn legacy_entries() -> Vec<Entry> {
    LEGACY
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn project(entries: &[Entry]) -> Projections {
    let mut projections = Projections::in_memory().unwrap();
    for (index, entry) in entries.iter().enumerate() {
        projections
            .apply(
                Lsn(u64::try_from(index).unwrap() + 1),
                u32::try_from(index).unwrap(),
                entry,
            )
            .unwrap();
    }
    projections
}

fn project_records(records: impl IntoIterator<Item = Record>) -> Projections {
    let entries: Vec<_> = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| entry(index, record))
        .collect();
    project(&entries)
}

/// The waiting tickets by title in serving order.
fn order(projections: &Projections) -> Vec<String> {
    let mut queued: Vec<_> = projections
        .tickets()
        .unwrap()
        .into_iter()
        .filter(|(view, _)| view.status == CardStatus::Claimed)
        .map(|(view, ticket)| (ticket.queue.unwrap(), view.title))
        .collect();
    queued.sort();
    queued.into_iter().map(|(_, title)| title).collect()
}

fn hire() -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id, workspace: at, ..
    } = &mut hired
    else {
        unreachable!("the sample is a hire");
    };
    *id = agent();
    *at = Some(dudes_proto::record::RecordedWorkspace {
        id: "service".into(),
        label: "Service".into(),
    });
    hired
}

fn kinds(projections: &Projections, n: u128) -> Vec<TicketHistoryKind> {
    projections
        .ticket_history(card(n))
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

/// The fixture was written by the build before priority existed: no record in it names one. Its
/// golden order comes from that build replaying it: rework first, then the position of each
/// assignment.
#[test]
fn a_journal_from_before_priority_keeps_its_age_order() {
    let entries = legacy_entries();
    assert_eq!(entries.len(), 14);
    assert!(!LEGACY.contains("priority"));
    let first = project(&entries);
    assert_eq!(order(&first), ["c5", "c3", "c1", "c4", "c2"]);
    for (_, ticket) in first.tickets().unwrap() {
        assert_eq!(ticket.priority, Priority::Unset);
        if let Some(key) = ticket.queue {
            assert_eq!(key.rank, 2);
        }
    }
    let second = project(&entries);
    assert_eq!(order(&second), order(&first));
    assert_eq!(second.export().unwrap(), first.export().unwrap());
}

/// Legacy records and priority-carrying ones in one journal: Rework first, then Urgent and High,
/// then Medium and Unset together by age, then Low. Every replay of it gives the same queue and
/// the same projection.
#[test]
fn legacy_and_prioritized_records_order_the_same_on_every_replay() {
    let mut entries = legacy_entries();
    let mut add = |record| {
        let index = entries.len();
        entries.push(entry(index, record));
    };
    add(created(6, Some(Priority::Low)));
    add(assigned(6, 0, None));
    add(created(7, Some(Priority::Urgent)));
    add(assigned(7, 0, None));
    add(edited(4, 1, "c4", Some(Priority::High)));
    add(assigned(1, 1, Some(Priority::Medium)));
    let expected = ["c5", "c7", "c4", "c3", "c2", "c1", "c6"];
    let first = project(&entries);
    assert_eq!(order(&first), expected);
    let (view, ticket) = first.ticket(card(7)).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Claimed);
    assert_eq!(ticket.queue.unwrap().rank, 0);
    assert_eq!(ticket.priority, Priority::Urgent);
    for _ in 0..2 {
        let again = project(&entries);
        assert_eq!(order(&again), expected);
        assert_eq!(again.export().unwrap(), first.export().unwrap());
    }
}

/// A priority edit on a ticket that is waiting moves it to the new rank and leaves the position
/// of its assignment alone, so clearing the priority puts it back where it stood.
#[test]
fn a_claimed_ticket_keeps_its_place_within_the_new_priority() {
    let mut records = vec![hire()];
    for n in 1..=3 {
        records.push(created(n, None));
    }
    for n in 1..=3 {
        records.push(assigned(n, 0, None));
    }
    let base = records.clone();
    let queue = |extra: Vec<Record>| order(&project_records(base.iter().cloned().chain(extra)));
    assert_eq!(queue(vec![]), ["c1", "c2", "c3"]);
    assert_eq!(
        queue(vec![edited(3, 1, "c3", Some(Priority::High))]),
        ["c3", "c1", "c2"]
    );
    assert_eq!(
        queue(vec![
            edited(3, 1, "c3", Some(Priority::High)),
            edited(1, 1, "c1", Some(Priority::High)),
        ]),
        ["c1", "c3", "c2"],
        "the earlier assignment goes first within High"
    );
    assert_eq!(
        queue(vec![
            edited(1, 1, "c1", Some(Priority::Low)),
            edited(2, 1, "c2", Some(Priority::Low)),
        ]),
        ["c3", "c1", "c2"],
        "Low waits behind the ticket with no priority, in assignment order"
    );
    assert_eq!(
        queue(vec![
            edited(3, 1, "c3", Some(Priority::Urgent)),
            edited(3, 2, "c3", Some(Priority::Unset)),
        ]),
        ["c1", "c2", "c3"],
        "clearing the priority restores the original place"
    );
}

/// Unset and Medium tickets share a rank, so they interleave by age, ahead of a Low ticket that
/// was assigned first.
#[test]
fn unset_and_medium_interleave_by_age_ahead_of_low() {
    let mut records = vec![hire()];
    let priorities = [
        (1, Some(Priority::Low)),
        (2, None),
        (3, Some(Priority::Medium)),
        (4, Some(Priority::Unset)),
        (5, Some(Priority::Medium)),
    ];
    for (n, priority) in priorities {
        records.push(created(n, priority));
    }
    for (n, _) in priorities {
        records.push(assigned(n, 0, None));
    }
    assert_eq!(
        order(&project_records(records)),
        ["c2", "c3", "c4", "c5", "c1"]
    );
}

/// Work sent back runs before new work whatever its priority, and keeps the priority it had.
#[test]
fn sent_back_work_keeps_its_priority_and_runs_first() {
    let projections = project_records([
        hire(),
        created(1, Some(Priority::Low)),
        created(2, Some(Priority::Urgent)),
        assigned(1, 0, None),
        started(1, GoalId::from_parts(8, 1), 1),
        moved(1, CardStatus::Doing, CardStatus::Review, 2, None),
        assigned(2, 0, None),
        assigned(1, 3, None),
    ]);
    assert_eq!(order(&projections), ["c1", "c2"]);
    let (_, ticket) = projections.ticket(card(1)).unwrap().unwrap();
    assert_eq!(ticket.priority, Priority::Low);
    let key = ticket.queue.unwrap();
    assert_eq!((key.class, key.rank), (QueueClass::Rework, 3));
}

/// An edit that changes both the words and the priority writes both events. An edit that changes
/// neither writes none, though it still advances the ticket's `seq`.
#[test]
fn history_tells_each_change_in_its_own_event() {
    let agent_id = agent();
    let goal_id = GoalId::from_parts(8, 1);
    let mut pass = samples::record(RecordKind::PredicateResult);
    if let Record::PredicateResult {
        goal,
        pass,
        provenance,
        evidence,
        ..
    } = &mut pass
    {
        *goal = goal_id;
        *pass = false;
        *provenance = dudes_types::Provenance::Observed;
        *evidence = dudes_proto::Evidence::Command {
            argv: vec!["cargo".into()],
            program: None,
            exit: Some(101),
            output_tail: String::new(),
        };
    }
    let mut passed = pass.clone();
    if let Record::PredicateResult { pass, evidence, .. } = &mut passed {
        *pass = true;
        *evidence = dudes_proto::Evidence::Command {
            argv: vec!["cargo".into()],
            program: None,
            exit: Some(0),
            output_tail: String::new(),
        };
    }
    let projections = project_records([
        hire(),
        created(1, Some(Priority::High)),
        edited(1, 0, "c1 renamed", Some(Priority::Low)),
        edited(1, 1, "c1 renamed", Some(Priority::Low)),
        edited(1, 2, "c1 renamed", None),
        assigned(1, 3, Some(Priority::Urgent)),
        started(1, goal_id, 4),
        pass,
        passed,
        moved(
            1,
            CardStatus::Doing,
            CardStatus::Blocked,
            5,
            Some(TicketReason::Slowed),
        ),
        assigned(1, 6, None),
    ]);
    assert_eq!(
        kinds(&projections, 1),
        [
            TicketHistoryKind::Created,
            TicketHistoryKind::PrioritySet {
                priority: Priority::High
            },
            TicketHistoryKind::Edited,
            TicketHistoryKind::PrioritySet {
                priority: Priority::Low
            },
            TicketHistoryKind::PrioritySet {
                priority: Priority::Urgent
            },
            TicketHistoryKind::Assigned { agent: agent_id },
            TicketHistoryKind::Started {
                agent: agent_id,
                round: 1
            },
            TicketHistoryKind::CheckPassed { exit: Some(0) },
            TicketHistoryKind::Moved {
                reason: TicketReason::Slowed
            },
            TicketHistoryKind::SentBack { agent: agent_id },
        ]
    );
    let history = projections.ticket_history(card(1)).unwrap();
    assert_eq!(history.total, 10);
    assert_eq!(history.omitted(), 0);
    assert_eq!(history.events[0].lsn, "2");
    assert_eq!(history.events[0].by, HistoryBy::Operator);
    assert_eq!(history.events[8].by, HistoryBy::Daemon);
}

#[test]
fn history_keeps_the_newest_48_and_counts_the_rest() {
    let mut records = vec![hire(), created(1, None)];
    for seq in 0..60 {
        records.push(edited(1, seq, &format!("title {seq}"), None));
    }
    let projections = project_records(records);
    let history = projections.ticket_history(card(1)).unwrap();
    assert_eq!((history.total, history.events.len()), (61, 48));
    assert_eq!(history.omitted(), 13);
    assert_eq!(history.events[0].lsn, "15");
    assert_eq!(history.events[47].lsn, "62");
    assert!(
        history
            .events
            .iter()
            .all(|event| event.kind == TicketHistoryKind::Edited)
    );
}

/// A ticket whose agent retires goes back to To do, and removing a ticket removes its history.
#[test]
fn retirement_returns_a_ticket_and_removal_drops_its_history() {
    let retire = Record::AgentRetired {
        id: agent(),
        reason: "finished".into(),
        browser_retire: None,
    };
    let projections = project_records([
        hire(),
        created(1, None),
        created(2, None),
        assigned(1, 0, None),
        retire,
    ]);
    assert_eq!(
        kinds(&projections, 1),
        [
            TicketHistoryKind::Created,
            TicketHistoryKind::Assigned { agent: agent() },
            TicketHistoryKind::Returned { agent: agent() },
        ]
    );
    assert_eq!(kinds(&projections, 2), [TicketHistoryKind::Created]);

    let removed = project_records([
        hire(),
        created(1, None),
        Record::TicketRemoved {
            card: card(1),
            by: operator(),
            expected_seq: Seq::ZERO,
        },
    ]);
    assert!(removed.ticket_history(card(1)).unwrap().is_empty());
}

/// A passed check is the work accepted when the operator moves the ticket to Done.
#[test]
fn accepting_a_ticket_is_one_event() {
    let projections = project_records([
        hire(),
        created(1, None),
        assigned(1, 0, None),
        started(1, GoalId::from_parts(8, 1), 1),
        moved(1, CardStatus::Doing, CardStatus::Review, 2, None),
        Record::CardTransition {
            card: card(1),
            from: CardStatus::Review,
            to: CardStatus::Done,
            by: operator(),
            expected_seq: Seq(3),
            reason: None,
        },
    ]);
    let history = projections.ticket_history(card(1)).unwrap();
    assert_eq!(
        history.events.last().unwrap().kind,
        TicketHistoryKind::Accepted
    );
    assert_eq!(history.events.last().unwrap().by, HistoryBy::Operator);
    assert_eq!(history.total, 4);
}
