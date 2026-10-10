use dudes_proto::{
    ActivityAgent, ActivityCard, ActivityEvent, ActivityView, CardView, Entry, LoopDecision,
    MAX_ACTIVITY_BYTES, MAX_ACTIVITY_ROWS, Record,
};
use dudes_types::{AgentId, CardId, CardStatus, Lsn};
use redb::{ReadableTable, Table};

use super::{AgentRow, RowTable, get_row};
use crate::error::{Error, Result};

const WINDOW: &str = "window";

pub(super) fn read(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<ActivityView>> {
    table
        .get(WINDOW)?
        .map(|value| serde_json::from_slice(value.value()).map_err(|source| Error::Row { source }))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn capture_before_source_update(
    table: &mut Table<'_, &'static str, &'static [u8]>,
    agents: &RowTable<'_>,
    board: &RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let Some(event) = event(agents, board, &entry.record)? else {
        return Ok(());
    };
    let row = ActivityView {
        lsn: lsn.0.to_string(),
        at: entry.at,
        event,
    };
    if serialize(std::slice::from_ref(&row))?.len() > MAX_ACTIVITY_BYTES {
        return Ok(());
    }
    let mut rows = read(table)?;
    rows.push(row);
    let mut bytes = serialize(&rows)?;
    while rows.len() > MAX_ACTIVITY_ROWS || bytes.len() > MAX_ACTIVITY_BYTES {
        rows.remove(0);
        bytes = serialize(&rows)?;
    }
    table.insert(WINDOW, bytes.as_slice())?;
    Ok(())
}

fn serialize(rows: &[ActivityView]) -> Result<Vec<u8>> {
    serde_json::to_vec(rows).map_err(|source| Error::Row { source })
}

fn agent(agents: &RowTable<'_>, id: AgentId) -> Result<ActivityAgent> {
    Ok(ActivityAgent {
        id,
        name: get_row::<AgentRow>(agents, &id.to_bytes())?.map(|row| row.name),
    })
}

fn card(board: &RowTable<'_>, id: CardId, status: CardStatus) -> Result<Option<ActivityCard>> {
    Ok(
        get_row::<CardView>(board, &id.to_bytes())?.map(|row| ActivityCard {
            id,
            title: row.title,
            status,
        }),
    )
}

fn event(
    agents: &RowTable<'_>,
    board: &RowTable<'_>,
    record: &Record,
) -> Result<Option<ActivityEvent>> {
    if let Some((id, _)) = record.goal_install() {
        return Ok(Some(ActivityEvent::GoalSet {
            agent: agent(agents, id)?,
        }));
    }
    Ok(Some(match record {
        Record::TurnIssued { agent: id, .. }
        | Record::NoteOnlyTurnIssued { agent: id, .. }
        | Record::ConversationTurnIssued { agent: id, .. }
        | Record::TaughtNoteOnlyTurnIssued { agent: id, .. }
        | Record::TurnIssuedWithBrowserNote { agent: id, .. } => ActivityEvent::TurnIssued {
            agent: agent(agents, *id)?,
        },
        Record::TurnEnded { agent: id, .. } => ActivityEvent::TurnEnded {
            agent: agent(agents, *id)?,
        },
        Record::LoopDecided {
            agent: id,
            decision: LoopDecision::Done,
            ..
        } => ActivityEvent::GoalDone {
            agent: agent(agents, *id)?,
        },
        Record::CardCreated { card, title, .. } => ActivityEvent::CardCreated {
            card: ActivityCard {
                id: *card,
                title: title.clone(),
                status: CardStatus::Todo,
            },
        },
        Record::Claim {
            card: id,
            agent: owner,
            ..
        } => {
            let Some(card) = card(board, *id, CardStatus::Claimed)? else {
                return Ok(None);
            };
            ActivityEvent::CardClaimed {
                card,
                agent: agent(agents, *owner)?,
            }
        }
        Record::CardTransition { card: id, to, .. } => {
            let Some(card) = card(board, *id, *to)? else {
                return Ok(None);
            };
            ActivityEvent::CardTransition { card }
        }
        Record::ConversationQuestion { agent: id, .. }
        | Record::HumanQuestion { agent: id, .. } => ActivityEvent::HumanQuestion {
            agent: agent(agents, *id)?,
        },
        Record::ConversationAnswerQueued { agent: id, .. }
        | Record::HumanAnswer { agent: id, .. } => ActivityEvent::HumanAnswer {
            agent: agent(agents, *id)?,
        },
        Record::AgentRenamed { id, name, .. } => ActivityEvent::AgentRenamed {
            agent: ActivityAgent {
                id: *id,
                name: Some(name.clone()),
            },
        },
        Record::AgentRetired { id, .. } => ActivityEvent::AgentRetired {
            agent: agent(agents, *id)?,
        },
        Record::Pause { agent: id, .. } => ActivityEvent::Pause {
            agent: agent(agents, *id)?,
        },
        Record::Resume { agent: id, .. } => ActivityEvent::Resume {
            agent: agent(agents, *id)?,
        },
        _ => return Ok(None),
    }))
}
