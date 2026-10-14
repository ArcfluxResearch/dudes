use dudes_proto::goal::Evidence;
use dudes_proto::ticket::summary_text;
use dudes_proto::{
    Actor, CardView, Entry, QueueClass, QueueKey, Record, TicketCheck, TicketCheckResult, TicketRow,
};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Lsn, Mode, Seq};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use super::turns::TurnRow;
use super::{RowTable, all_rows, get_row, put_row};
use crate::error::Result;

#[derive(Serialize, Deserialize)]
pub(super) struct BoardRow {
    #[serde(flatten)]
    pub(super) view: CardView,
    #[serde(default)]
    pub(super) claimed_lsn: Option<Lsn>,
    /// Present only for a ticket. Stored under its own name because the flattened card has a
    /// `ticket` of its own.
    #[serde(
        default,
        rename = "ticket_row",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) ticket: Option<TicketRow>,
}

impl BoardRow {
    /// The card is no longer held, and a ticket leaves its queue.
    fn release(&mut self) {
        self.view.claimed_by = None;
        self.claimed_lsn = None;
        if let Some(ticket) = &mut self.ticket {
            ticket.queue = None;
        }
    }
}

pub(super) fn apply(
    table: &mut RowTable<'_>,
    turns: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    match &entry.record {
        Record::CardCreated {
            card,
            title,
            department,
            by,
            deps,
        } => put_row(
            table,
            &card.to_bytes(),
            &BoardRow {
                view: CardView {
                    id: *card,
                    title: title.clone(),
                    department: department.clone(),
                    status: CardStatus::Todo,
                    status_by: *by,
                    seq: Seq::ZERO,
                    claimed_by: None,
                    deps: deps.clone(),
                    ticket: None,
                },
                claimed_lsn: None,
                ticket: None,
            },
        ),
        Record::Claim {
            card,
            agent,
            expected_seq,
        } => update(table, lsn, *card, |row| {
            row.view.status = CardStatus::Claimed;
            row.view.status_by = Actor::Agent { agent: *agent };
            row.view.seq = expected_seq.next();
            row.view.claimed_by = Some(*agent);
            row.claimed_lsn = Some(lsn);
        }),
        Record::CardTransition {
            card,
            to,
            by,
            expected_seq,
            reason,
            ..
        } => update(table, lsn, *card, |row| {
            row.view.status = *to;
            row.view.status_by = *by;
            row.view.seq = expected_seq.next();
            if let Some(ticket) = &mut row.ticket {
                ticket.reason = *reason;
                if *to != CardStatus::Claimed {
                    ticket.queue = None;
                }
                if *to == CardStatus::Todo {
                    row.release();
                }
            }
        }),
        Record::AgentRetired { id, .. } => {
            for mut card in all_rows::<BoardRow>(table)? {
                if card.view.claimed_by == Some(*id) {
                    // A pending decision is not erased by the departure of the agent that did
                    // the work: the ticket stays Ready for you and remembers who did it.
                    if card.ticket.is_some() && card.view.status == CardStatus::Review {
                        continue;
                    }
                    card.release();
                    card.view.seq = card.view.seq.next();
                    if card.view.status != CardStatus::Done {
                        card.view.status = CardStatus::Todo;
                        card.view.status_by = Actor::System;
                    }
                    put_row(table, &card.view.id.to_bytes(), &card)?;
                }
            }
            Ok(())
        }
        Record::TurnEnded { .. } | Record::PredicateResult { .. } => {
            apply_outcome(table, turns, entry)
        }
        _ => apply_ticket(table, lsn, &entry.record),
    }
}

/// What the daemon learned about a ticket's goal: the agent's last words and the check's result.
fn apply_outcome(
    table: &mut RowTable<'_>,
    turns: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    entry: &Entry,
) -> Result<()> {
    match &entry.record {
        Record::TurnEnded {
            turn,
            mode: Mode::Managed,
            last_message,
            ..
        } => {
            let goal = get_row::<TurnRow>(turns, &turn.to_bytes())?.and_then(|row| row.goal);
            match goal {
                Some(goal) => update_goal_ticket(table, goal, |ticket| {
                    ticket.summary = last_message.as_deref().map(summary_text);
                }),
                None => Ok(()),
            }
        }
        Record::PredicateResult {
            goal,
            pass,
            evidence,
            ..
        } => update_goal_ticket(table, *goal, |ticket| {
            ticket.check = Some(TicketCheckResult {
                pass: *pass,
                exit: match evidence {
                    Evidence::Command { exit, .. } => *exit,
                    _ => None,
                },
                at: entry.at,
            });
        }),
        _ => Ok(()),
    }
}

/// Change the ticket whose newest start installed `goal`, when there is one.
fn update_goal_ticket(
    table: &mut RowTable<'_>,
    goal: GoalId,
    change: impl FnOnce(&mut TicketRow),
) -> Result<()> {
    let found = all_rows::<BoardRow>(table)?.into_iter().find(|row| {
        row.ticket
            .as_ref()
            .is_some_and(|ticket| ticket.goal == Some(goal))
    });
    let Some(mut row) = found else {
        return Ok(());
    };
    if let Some(ticket) = &mut row.ticket {
        change(ticket);
    }
    put_row(table, &row.view.id.to_bytes(), &row)
}

/// The five ticket records: a ticket is a card with more to remember.
fn apply_ticket(table: &mut RowTable<'_>, lsn: Lsn, record: &Record) -> Result<()> {
    match record {
        Record::TicketCreated {
            card,
            title,
            description,
            department,
            project,
            done_when,
            by,
        } => {
            let view = CardView {
                id: *card,
                title: title.clone(),
                department: department.clone(),
                status: CardStatus::Todo,
                status_by: *by,
                seq: Seq::ZERO,
                claimed_by: None,
                deps: Vec::new(),
                ticket: None,
            };
            let ticket = TicketRow {
                description: description.clone(),
                project: project.clone(),
                done_when: done_when.clone(),
                goal: None,
                queue: None,
                message: None,
                round: 0,
                reason: None,
                summary: None,
                check: None,
                instruction_review: None,
            };
            let row = BoardRow {
                view,
                claimed_lsn: None,
                ticket: Some(ticket),
            };
            put_row(table, &card.to_bytes(), &row)
        }
        Record::TicketEdited {
            card,
            title,
            description,
            done_when,
            expected_seq,
            ..
        } => update_ticket(table, lsn, *card, |row| {
            edit_ticket(row, title, description, done_when, *expected_seq);
        }),
        Record::TicketAssigned {
            card,
            agent,
            by,
            message,
            expected_seq,
        } => update_ticket(table, lsn, *card, |row| {
            queue_ticket(row, lsn, (*agent, *by), message.as_deref(), *expected_seq);
        }),
        Record::TicketStarted {
            card,
            agent,
            goal,
            expected_seq,
        } => update_ticket(table, lsn, *card, |row| {
            start_ticket(row, lsn, *agent, goal.id, *expected_seq);
        }),
        Record::TicketRemoved { card, .. } => remove_ticket(table, lsn, *card),
        _ => Ok(()),
    }
}

pub(super) fn apply_instruction(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
    state: &dudes_proto::instruction_state::InstructionState,
    folded: &dudes_proto::instruction_state::InstructionFold,
) -> Result<()> {
    let mut cards = folded.evidence_cards.clone();
    if let Some((card, _)) = &folded.reviewed
        && !cards.contains(card)
    {
        cards.push(*card);
    }
    match &entry.record {
        Record::CardTransition { card, .. }
        | Record::TicketAssigned { card, .. }
        | Record::TicketStarted { card, .. }
        | Record::TicketEdited { card, .. } => {
            if !cards.contains(card) {
                cards.push(*card);
            }
        }
        Record::AgentRetired { .. } => {
            for card in state.pending_cards() {
                if !cards.contains(&card) {
                    cards.push(card);
                }
            }
        }
        _ => {}
    }
    for card in cards {
        let Some(mut row) = get_row::<BoardRow>(table, &card.to_bytes())? else {
            continue;
        };
        if row.ticket.is_none() {
            continue;
        }
        if let Some((reviewed, action)) = &folded.reviewed
            && *reviewed == card
        {
            let Record::TicketInstructionReviewed {
                by, expected_seq, ..
            } = &entry.record
            else {
                continue;
            };
            review_ticket(&mut row, lsn, *by, *expected_seq, action);
        }
        if folded.evidence_cards.contains(&card) {
            row.view.seq = row.view.seq.next();
        }
        let review = state
            .review(card, super::instructions::hash)
            .map_err(|reason| crate::Error::Instruction { lsn, reason })?;
        let reason = state.ticket_reason(card);
        if let Some(ticket) = &mut row.ticket {
            ticket.instruction_review = review;
            if let Some(reason) = reason {
                ticket.reason = Some(reason);
                row.view.status = CardStatus::Blocked;
                row.view.status_by = Actor::System;
            }
            row.view.ticket = Some(dudes_proto::CardTicket {
                position: None,
                round: ticket.round,
                reason: ticket.reason,
                stopping: false,
                instruction_review: ticket
                    .instruction_review
                    .as_ref()
                    .map(dudes_proto::InstructionPendingReview::summary),
            });
        }
        put_row(table, &card.to_bytes(), &row)?;
    }
    Ok(())
}

fn review_ticket(
    row: &mut BoardRow,
    lsn: Lsn,
    by: Actor,
    expected_seq: Seq,
    action: &dudes_proto::InstructionReviewAction,
) {
    match action {
        dudes_proto::InstructionReviewAction::ChangesRequested { agent, message, .. } => {
            queue_ticket(row, lsn, (*agent, by), message.as_deref(), expected_seq);
        }
        dudes_proto::InstructionReviewAction::Accept { completion } => {
            row.view.seq = expected_seq.next();
            row.view.status_by = by;
            let ticket = row.ticket.as_mut().expect("ticket was checked");
            ticket.reason = None;
            row.view.status = match completion {
                dudes_proto::InstructionAcceptCompletion::Completed { .. } => CardStatus::Done,
                dudes_proto::InstructionAcceptCompletion::Continue => {
                    if ticket.queue.is_some() {
                        CardStatus::Claimed
                    } else if ticket.goal.is_some() && row.view.claimed_by.is_some() {
                        CardStatus::Doing
                    } else {
                        CardStatus::Todo
                    }
                }
            };
        }
    }
}

/// An edit replaces the words and the check, and leaves the status, holder and queue place alone.
fn edit_ticket(
    row: &mut BoardRow,
    title: &str,
    description: &str,
    done_when: &TicketCheck,
    expected_seq: Seq,
) {
    title.clone_into(&mut row.view.title);
    row.view.seq = expected_seq.next();
    if let Some(ticket) = &mut row.ticket {
        description.clone_into(&mut ticket.description);
        ticket.done_when = done_when.clone();
    }
}

/// An assignment queues the ticket with `holder` (the agent, then who assigned it). A ticket
/// that is already queued keeps its class, so work sent back stays ahead of new work wherever it
/// is moved, and it keeps its message unless the assignment carries a new one.
fn queue_ticket(
    row: &mut BoardRow,
    lsn: Lsn,
    holder: (AgentId, Actor),
    message: Option<&str>,
    expected_seq: Seq,
) {
    let (agent, by) = holder;
    let kept = row.ticket.as_ref().and_then(|ticket| ticket.queue);
    let class = match (row.view.status, kept) {
        (CardStatus::Claimed, Some(key)) => key.class,
        (CardStatus::Review | CardStatus::Blocked, _) => QueueClass::Rework,
        _ => QueueClass::New,
    };
    row.view.status = CardStatus::Claimed;
    row.view.status_by = by;
    row.view.seq = expected_seq.next();
    row.view.claimed_by = Some(agent);
    row.claimed_lsn = Some(lsn);
    if let Some(ticket) = &mut row.ticket {
        // The previous round's goal, and its Done, belong to that round: only the next start
        // names the goal that can bring the ticket back to Ready for you.
        ticket.goal = None;
        ticket.queue = Some(QueueKey { class, lsn });
        if let Some(message) = message {
            ticket.message = Some(message.to_owned());
        }
        ticket.reason = None;
    }
}

/// A start moves the card to Doing, links its goal and spends the stored message.
fn start_ticket(row: &mut BoardRow, lsn: Lsn, agent: AgentId, goal: GoalId, expected_seq: Seq) {
    row.view.status = CardStatus::Doing;
    row.view.status_by = Actor::System;
    row.view.seq = expected_seq.next();
    row.view.claimed_by = Some(agent);
    row.claimed_lsn.get_or_insert(lsn);
    if let Some(ticket) = &mut row.ticket {
        ticket.goal = Some(goal);
        ticket.queue = None;
        ticket.message = None;
        ticket.reason = None;
        ticket.summary = None;
        ticket.check = None;
        ticket.round = ticket.round.saturating_add(1);
    }
}

/// A removal takes the ticket off the board; a plain card is left alone.
fn remove_ticket(table: &mut RowTable<'_>, lsn: Lsn, card: CardId) -> Result<()> {
    let is_ticket =
        get_row::<BoardRow>(table, &card.to_bytes())?.is_some_and(|row| row.ticket.is_some());
    if is_ticket {
        table.remove(&card.to_bytes())?;
    } else {
        tracing::warn!(target: "journal", %lsn, %card, "projection.not_a_ticket");
    }
    Ok(())
}

/// Change one card's row, or note that the record names no card.
fn update(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    card: CardId,
    change: impl FnOnce(&mut BoardRow),
) -> Result<()> {
    let Some(mut row) = get_row::<BoardRow>(table, &card.to_bytes())? else {
        tracing::warn!(target: "journal", %lsn, %card, "projection.unknown_card");
        return Ok(());
    };
    change(&mut row);
    put_row(table, &card.to_bytes(), &row)
}

/// As [`update`], for a record that only a ticket can receive: a plain card is left as it is.
fn update_ticket(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    card: CardId,
    change: impl FnOnce(&mut BoardRow),
) -> Result<()> {
    update(table, lsn, card, |row| {
        if row.ticket.is_some() {
            change(row);
        } else {
            tracing::warn!(target: "journal", %lsn, %card, "projection.not_a_ticket");
        }
    })
}
