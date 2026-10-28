//! Each ticket's bounded history, folded from the same records the board folds. It is read before
//! the board changes a row, because a ticket sent back, edited or returned is told apart by the
//! row it arrives at.

use dudes_proto::goal::Evidence;
use dudes_proto::{
    Actor, Entry, HistoryBy, InstructionAcceptCompletion, InstructionReviewAction, Record,
    TicketHistory, TicketHistoryEvent, TicketHistoryKind, TicketReason, TicketRow,
};
use dudes_types::{AgentId, CardId, CardStatus, Lsn};
use redb::{ReadableTable, Table};

use super::board::BoardRow;
use super::{RowTable, all_rows, get_row};
use crate::error::{Error, Result};

type HistoryTable<'txn> = Table<'txn, &'static [u8; 16], &'static [u8]>;

/// A card's status and Needs-you reason as they stood before a record changed them.
pub(super) type Seen = Vec<(CardId, Option<(CardStatus, Option<TicketReason>)>)>;

/// What the caller already folded for this record.
#[derive(Clone, Copy)]
pub(super) struct Folded<'a> {
    /// The round a ticket start opens.
    pub(super) round: Option<u32>,
    /// The instruction review action the record applies.
    pub(super) reviewed: Option<&'a (CardId, InstructionReviewAction)>,
}

pub(super) fn read(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    card: CardId,
) -> Result<TicketHistory> {
    Ok(get_row::<TicketHistory>(table, &card.to_bytes())?.unwrap_or_default())
}

pub(super) fn seen(board: &RowTable<'_>, cards: &[CardId]) -> Result<Seen> {
    cards
        .iter()
        .map(|card| {
            let row = get_row::<BoardRow>(board, &card.to_bytes())?;
            Ok((
                *card,
                row.and_then(|row| row.ticket.map(|ticket| (row.view.status, ticket.reason))),
            ))
        })
        .collect()
}

fn ticket(board: &RowTable<'_>, card: CardId) -> Result<Option<(BoardRow, TicketRow)>> {
    Ok(
        get_row::<BoardRow>(board, &card.to_bytes())?.and_then(|mut row| {
            let ticket = row.ticket.take()?;
            Some((row, ticket))
        }),
    )
}

fn by(actor: Actor) -> HistoryBy {
    HistoryBy::from(actor)
}

type Events = Vec<(CardId, HistoryBy, TicketHistoryKind)>;

fn created_events(lsn: Lsn, record: &Record) -> Events {
    let Some(created) = record.ticket_created() else {
        return Vec::new();
    };
    if !matches!(
        dudes_proto::repository_state::fold(
            lsn,
            record,
            dudes_proto::repository_state::RepositoryFacts::default(),
        ),
        dudes_proto::repository_state::RepositoryFold::Ticket(_)
    ) {
        return Vec::new();
    }
    let mut out = vec![(created.card, by(created.by), TicketHistoryKind::Created)];
    if let Some(priority) = created.priority.filter(|priority| !priority.is_unset()) {
        out.push((
            created.card,
            by(created.by),
            TicketHistoryKind::PrioritySet { priority },
        ));
    }
    out
}

fn edited_events(board: &RowTable<'_>, record: &Record) -> Result<Events> {
    let Some(edited) = record.ticket_edited() else {
        return Ok(Vec::new());
    };
    let Some((row, ticket)) = ticket(board, edited.card)? else {
        return Ok(Vec::new());
    };
    let text = edited.title != row.view.title
        || edited.description != ticket.description
        || *edited.done_when != ticket.done_when;
    let priority = edited
        .priority
        .filter(|priority| *priority != ticket.priority);
    let mut out = Vec::new();
    if text {
        out.push((edited.card, by(edited.by), TicketHistoryKind::Edited));
    }
    if let Some(priority) = priority {
        out.push((
            edited.card,
            by(edited.by),
            TicketHistoryKind::PrioritySet { priority },
        ));
    }
    Ok(out)
}

fn assigned_events(board: &RowTable<'_>, record: &Record) -> Result<Events> {
    let Record::TicketAssigned {
        card,
        agent,
        by: actor,
        priority,
        ..
    } = record
    else {
        return Ok(Vec::new());
    };
    let Some((row, ticket)) = ticket(board, *card)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    if let Some(priority) = priority.filter(|priority| *priority != ticket.priority) {
        out.push((
            *card,
            by(*actor),
            TicketHistoryKind::PrioritySet { priority },
        ));
    }
    let kind = if matches!(row.view.status, CardStatus::Review | CardStatus::Blocked) {
        TicketHistoryKind::SentBack { agent: *agent }
    } else {
        TicketHistoryKind::Assigned { agent: *agent }
    };
    out.push((*card, by(*actor), kind));
    Ok(out)
}

fn reviewed_events(board: &RowTable<'_>, actor: Actor, folded: Folded<'_>) -> Result<Events> {
    let Some((card, action)) = folded.reviewed else {
        return Ok(Vec::new());
    };
    if ticket(board, *card)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(match action {
        InstructionReviewAction::ChangesRequested { agent, .. } => vec![(
            *card,
            by(actor),
            TicketHistoryKind::SentBack { agent: *agent },
        )],
        InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Completed { .. },
        } => vec![(*card, by(actor), TicketHistoryKind::Accepted)],
        InstructionReviewAction::Accept { .. } => Vec::new(),
    })
}

fn check_events(board: &RowTable<'_>, record: &Record) -> Result<Events> {
    let Record::PredicateResult {
        goal,
        pass,
        evidence,
        ..
    } = record
    else {
        return Ok(Vec::new());
    };
    let Some(row) = all_rows::<BoardRow>(board)?.into_iter().find(|row| {
        row.ticket
            .as_ref()
            .is_some_and(|ticket| ticket.goal == Some(*goal))
    }) else {
        return Ok(Vec::new());
    };
    let exit = match evidence {
        Evidence::Command { exit, .. } => *exit,
        _ => None,
    };
    let kind = if *pass {
        TicketHistoryKind::CheckPassed { exit }
    } else {
        TicketHistoryKind::CheckFailed { exit }
    };
    Ok(vec![(row.view.id, HistoryBy::Daemon, kind)])
}

fn retired_events(board: &RowTable<'_>, agent: AgentId) -> Result<Events> {
    Ok(all_rows::<BoardRow>(board)?
        .into_iter()
        .filter(|row| {
            row.ticket.is_some()
                && row.view.claimed_by == Some(agent)
                && !matches!(row.view.status, CardStatus::Review | CardStatus::Done)
        })
        .map(|row| {
            (
                row.view.id,
                HistoryBy::Daemon,
                TicketHistoryKind::Returned { agent },
            )
        })
        .collect())
}

fn asked_events(board: &RowTable<'_>, lsn: Lsn, agent: AgentId) -> Result<Events> {
    Ok(all_rows::<BoardRow>(board)?
        .into_iter()
        .filter(|row| {
            row.ticket.is_some()
                && row.view.claimed_by == Some(agent)
                && row.view.status == CardStatus::Doing
        })
        .map(|row| {
            (
                row.view.id,
                HistoryBy::Agent,
                TicketHistoryKind::Asked {
                    agent,
                    question_lsn: lsn.0.to_string(),
                },
            )
        })
        .collect())
}

/// The ticket holding `agent`'s newest `asked` event that no `answered` event on that ticket
/// closes, with that question's identity. The scan reads only the history table, so a replay
/// finds the same ticket.
fn unanswered(table: &HistoryTable<'_>, agent: AgentId) -> Result<Option<(CardId, String)>> {
    let mut newest: Option<(u64, CardId, String)> = None;
    for entry in table.iter()? {
        let (key, value) = entry?;
        let history: TicketHistory =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        for event in &history.events {
            let TicketHistoryKind::Asked {
                agent: asker,
                question_lsn,
            } = &event.kind
            else {
                continue;
            };
            let answered = history.events.iter().any(|later| {
                matches!(
                    &later.kind,
                    TicketHistoryKind::Answered { agent: who, question_lsn: asked }
                        if *who == agent && asked == question_lsn
                )
            });
            let Ok(position) = question_lsn.parse::<u64>() else {
                continue;
            };
            if *asker == agent
                && !answered
                && newest.as_ref().is_none_or(|(best, _, _)| position > *best)
            {
                newest = Some((
                    position,
                    CardId::from_bytes(*key.value()),
                    question_lsn.clone(),
                ));
            }
        }
    }
    Ok(newest.map(|(_, card, question_lsn)| (card, question_lsn)))
}

/// The events `entry` causes, before the board folds it.
fn events(board: &RowTable<'_>, lsn: Lsn, entry: &Entry, folded: Folded<'_>) -> Result<Events> {
    let record = &entry.record;
    if record.ticket_created().is_some() {
        return Ok(created_events(lsn, record));
    }
    if record.ticket_edited().is_some() {
        return edited_events(board, record);
    }
    if let Some(start) = record.ticket_start() {
        return Ok(match (folded.round, ticket(board, start.card)?) {
            (Some(round), Some(_)) => vec![(
                start.card,
                HistoryBy::Daemon,
                TicketHistoryKind::Started {
                    agent: start.agent,
                    round,
                },
            )],
            _ => Vec::new(),
        });
    }
    match record {
        Record::TicketAssigned { .. } => assigned_events(board, record),
        Record::TicketInstructionReviewed { by, .. } => reviewed_events(board, *by, folded),
        Record::CardTransition {
            card,
            to: CardStatus::Done,
            by: actor,
            ..
        } => Ok(if ticket(board, *card)?.is_some() {
            vec![(*card, by(*actor), TicketHistoryKind::Accepted)]
        } else {
            Vec::new()
        }),
        Record::PredicateResult { .. } => check_events(board, record),
        Record::AgentRetired { id, .. } => retired_events(board, *id),
        Record::HumanQuestion { agent, .. } => asked_events(board, lsn, *agent),
        _ => Ok(Vec::new()),
    }
}

fn append(table: &mut HistoryTable<'_>, card: CardId, event: TicketHistoryEvent) -> Result<()> {
    let mut history = read(table, card)?;
    history.push(event);
    let bytes = serde_json::to_vec(&history).map_err(|source| Error::Row { source })?;
    table.insert(&card.to_bytes(), bytes.as_slice())?;
    Ok(())
}

fn event(lsn: Lsn, entry: &Entry, by: HistoryBy, kind: TicketHistoryKind) -> TicketHistoryEvent {
    TicketHistoryEvent {
        lsn: lsn.0.to_string(),
        at: entry.at,
        by,
        kind,
    }
}

/// Fold `entry` into the histories, from the board as it stood before the record.
pub(super) fn capture(
    table: &mut HistoryTable<'_>,
    board: &RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
    folded: Folded<'_>,
) -> Result<()> {
    match &entry.record {
        Record::TicketRemoved { card, .. } => {
            if ticket(board, *card)?.is_some() {
                table.remove(&card.to_bytes())?;
            }
            return Ok(());
        }
        Record::HumanAnswer { agent, .. } => {
            if let Some((card, question_lsn)) = unanswered(table, *agent)? {
                let kind = TicketHistoryKind::Answered {
                    agent: *agent,
                    question_lsn,
                };
                append(table, card, event(lsn, entry, HistoryBy::Operator, kind))?;
            }
            return Ok(());
        }
        record => {
            if let Some(created) = record.ticket_created() {
                table.remove(&created.card.to_bytes())?;
            }
        }
    }
    for (card, by, kind) in events(board, lsn, entry, folded)? {
        append(table, card, event(lsn, entry, by, kind))?;
    }
    Ok(())
}

/// Record the moves into Needs you that the board made while folding `entry`, whether a
/// transition carried the reason or the review of shared instructions did.
pub(super) fn capture_moves(
    table: &mut HistoryTable<'_>,
    board: &RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
    seen: Seen,
) -> Result<()> {
    for (card, before) in seen {
        let Some((row, ticket)) = ticket(board, card)? else {
            continue;
        };
        let Some(reason) = ticket
            .reason
            .filter(|_| row.view.status == CardStatus::Blocked)
        else {
            continue;
        };
        if before.is_some_and(|(status, was)| status == CardStatus::Blocked && was == Some(reason))
        {
            continue;
        }
        append(
            table,
            card,
            event(
                lsn,
                entry,
                by(row.view.status_by),
                TicketHistoryKind::Moved { reason },
            ),
        )?;
    }
    Ok(())
}

pub(super) fn export(
    lines: &mut Vec<String>,
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
) -> Result<()> {
    for entry in table.iter()? {
        let (key, value) = entry?;
        let history: TicketHistory =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&history).map_err(|source| Error::Row { source })?;
        lines.push(format!(
            "ticket_history {} {json}",
            super::hex(key.value().as_slice())
        ));
    }
    Ok(())
}
