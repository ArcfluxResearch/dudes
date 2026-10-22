use std::collections::BTreeMap;

use dudes_proto::{
    BrowserNoteDestination, ChatContextState, ChatContextView, ChatPendingState, ChatPendingView,
    ChatReason, ChatTurnState, ChatTurnView, ChatView, Entry, LoopDecision, MAX_CHAT_BYTES,
    MAX_CHAT_CONTEXT, MAX_CHAT_CONTEXT_BYTES, MAX_CHAT_CONTEXT_PER_AGENT, MAX_CHAT_PENDING,
    MAX_CHAT_PENDING_BYTES, MAX_CHAT_PENDING_PER_AGENT, MAX_CHAT_TEXT_BYTES, MAX_CHAT_TURN_BYTES,
    MAX_CHAT_TURN_BYTES_PER_AGENT, MAX_CHAT_TURNS, MAX_CHAT_TURNS_PER_AGENT, NoteAdmission,
    NoteContract, Record, TurnOutcome,
};
use dudes_types::{AgentId, GoalId, Lsn, TurnId};
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};

use super::{AgentRow, RowTable, get_row, put_row};
use crate::error::{Error, Result};

/// Run-local proof that refines a durable pending-note view without changing journal truth.
#[derive(Clone, Copy, Debug)]
pub struct ChatOverlay {
    /// Past delivery is unresolved after restart.
    pub uncertain: bool,
    /// The current run's actual host dispatch or context emission has claimed delivery.
    pub delivery_started: bool,
    /// A fixed current admission reason, never provider error text.
    pub reason: Option<ChatReason>,
}

pub(super) type NoteTable<'txn> = Table<'txn, u64, &'static [u8]>;
pub(super) type WindowTable<'txn> = Table<'txn, &'static str, &'static [u8]>;

pub(super) struct Tables<'txn> {
    pub notes: NoteTable<'txn>,
    pub issues: RowTable<'txn>,
    pub decisions: RowTable<'txn>,
    pub completions: RowTable<'txn>,
    pub windows: WindowTable<'txn>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum NoteState {
    Queued,
    Reserved(TurnId),
    Delivered,
    Cancelled,
    ContextEmitted,
    ReissueCancelled(TurnId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Note {
    agent: AgentId,
    text: String,
    task: Option<String>,
    answer_to: Option<Lsn>,
    destination: BrowserNoteDestination,
    state: NoteState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Cancellation {
    Clean(Lsn),
    Ambiguous { lsn: Lsn, intent: Lsn },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BrowserTurn {
    agent: AgentId,
    goal: Option<GoalId>,
    note_lsn: Lsn,
    issue_lsn: Lsn,
    note_only: bool,
    completion_lsn: Option<Lsn>,
    contract: Option<NoteContract>,
    intents: Vec<Lsn>,
    uncertain_dispatches: Vec<Lsn>,
    spawn: Option<Lsn>,
    end: Option<Lsn>,
    cancellation: Option<Cancellation>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Decisions {
    ordinary: Option<(Lsn, GoalId, bool)>,
    #[serde(default)]
    conversation: Option<(Lsn, Lsn, Lsn, bool)>,
    note: Option<NoteDecision>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NoteDecision {
    lsn: Lsn,
    goal: GoalId,
    note_lsn: Lsn,
    completion_lsn: Lsn,
    issued: bool,
    reason: Option<ChatReason>,
    contract: NoteContract,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnHistory {
    issue_lsn: Lsn,
    view: ChatTurnView,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContextHistory {
    emission_lsn: Lsn,
    view: ChatContextView,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Windows {
    turns: Vec<TurnHistory>,
    context: Vec<ContextHistory>,
}

fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| Error::Row { source })
}

fn invalid() -> Error {
    Error::Row {
        source: <serde_json::Error as serde::de::Error>::custom(
            "invalid browser-note lifecycle join",
        ),
    }
}

fn note(table: &impl ReadableTable<u64, &'static [u8]>, lsn: Lsn) -> Result<Note> {
    table
        .get(lsn.0)?
        .map(|value| serde_json::from_slice(value.value()).map_err(|source| Error::Row { source }))
        .transpose()?
        .ok_or_else(invalid)
}

fn windows(table: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Windows> {
    table
        .get("window")?
        .map(|value| serde_json::from_slice(value.value()).map_err(|source| Error::Row { source }))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn require_issue(
    tables: &Tables<'_>,
    agent: AgentId,
    note_lsn: Lsn,
    turn: TurnId,
) -> Result<BrowserTurn> {
    let issue = get_row::<BrowserTurn>(&tables.issues, &turn.to_bytes())?.ok_or_else(invalid)?;
    if issue.agent != agent || issue.note_lsn != note_lsn {
        return Err(invalid());
    }
    Ok(issue)
}

fn public_pending(lsn: Lsn, note: &Note) -> Option<ChatPendingView> {
    let (state, turn_id, cancellable, reason) = match note.state {
        NoteState::Queued => (ChatPendingState::Queued, None, true, None),
        NoteState::Reserved(turn) => (ChatPendingState::Reserved, Some(turn), true, None),
        NoteState::ReissueCancelled(turn) => (
            ChatPendingState::DeliveryUncertain,
            Some(turn),
            false,
            Some(ChatReason::DeliveryUncertain),
        ),
        NoteState::Delivered | NoteState::Cancelled | NoteState::ContextEmitted => return None,
    };
    Some(ChatPendingView {
        agent_id: note.agent,
        note_lsn: lsn.0.to_string(),
        goal_id: match note.destination {
            BrowserNoteDestination::ManagedGoal { goal } => Some(goal),
            BrowserNoteDestination::AttachedContext
            | BrowserNoteDestination::ManagedConversation { .. } => None,
        },
        text: note.text.clone(),
        state,
        turn_id,
        cancellable,
        reason,
    })
}

fn pending_rows(
    notes: &impl ReadableTable<u64, &'static [u8]>,
    agents: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
) -> Result<Vec<ChatPendingView>> {
    let mut rows = Vec::new();
    for entry in notes.iter()? {
        let (key, value) = entry?;
        let note: Note =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        if get_row::<AgentRow>(agents, &note.agent.to_bytes())?.is_some()
            && let Some(row) = public_pending(Lsn(key.value()), &note)
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Whether another pending row fits every admission bound after the largest live overlay.
pub fn admits_pending(rows: &[ChatPendingView], candidate: &ChatPendingView) -> Result<bool> {
    if candidate.text.is_empty()
        || candidate.text.len() > MAX_CHAT_TEXT_BYTES
        || rows.len() >= MAX_CHAT_PENDING
        || rows
            .iter()
            .filter(|row| row.agent_id == candidate.agent_id)
            .count()
            >= MAX_CHAT_PENDING_PER_AGENT
    {
        return Ok(false);
    }
    let mut bytes = 2 + rows.len();
    for row in rows.iter().chain(std::iter::once(candidate)) {
        bytes += row
            .reserved_bytes()
            .map_err(|source| Error::Row { source })?;
    }
    Ok(bytes <= MAX_CHAT_PENDING_BYTES)
}

fn array_bytes<T: Serialize>(rows: &[T]) -> Result<usize> {
    encoded(&rows).map(|bytes| bytes.len())
}

fn history_bytes(rows: &[&ChatTurnView]) -> Result<usize> {
    let longest = encoded(&ChatTurnState::DeliveryUncertain)?.len();
    let mut bytes = array_bytes(rows)?;
    for row in rows {
        bytes += longest - encoded(&row.state)?.len();
    }
    Ok(bytes)
}

fn reserved_object_bytes(view: &ChatView) -> Result<usize> {
    let mut pending = 2 + view.pending.len().saturating_sub(1);
    for row in &view.pending {
        pending += row
            .reserved_bytes()
            .map_err(|source| Error::Row { source })?;
    }
    let turns: Vec<_> = view.turns.iter().collect();
    Ok(
        encoded(view)?.len() + b",\"chat\":".len() - array_bytes(&view.pending)? + pending
            - array_bytes(&view.turns)?
            + history_bytes(&turns)?,
    )
}

fn trim_windows(window: &mut Windows) -> Result<()> {
    let mut fitting = Vec::new();
    for row in std::mem::take(&mut window.turns) {
        if history_bytes(&[&row.view])? <= MAX_CHAT_TURN_BYTES_PER_AGENT {
            fitting.push(row);
        }
    }
    window.turns = fitting;
    window.turns.sort_by_key(|row| row.issue_lsn);
    loop {
        let mut by_agent = BTreeMap::<_, Vec<_>>::new();
        for row in &window.turns {
            by_agent
                .entry(row.view.agent_id)
                .or_default()
                .push(&row.view);
        }
        let mut oversized = None;
        for (agent, same) in by_agent {
            if same.len() > MAX_CHAT_TURNS_PER_AGENT
                || history_bytes(&same)? > MAX_CHAT_TURN_BYTES_PER_AGENT
            {
                oversized = Some(agent);
                break;
            }
        }
        let global: Vec<_> = window.turns.iter().map(|row| &row.view).collect();
        if let Some(agent) = oversized {
            let first = window
                .turns
                .iter()
                .position(|row| row.view.agent_id == agent)
                .expect("oversized agent exists");
            window.turns.remove(first);
        } else if window.turns.len() > MAX_CHAT_TURNS
            || history_bytes(&global)? > MAX_CHAT_TURN_BYTES
        {
            window.turns.remove(0);
        } else {
            break;
        }
    }
    window.context.sort_by_key(|row| row.emission_lsn);
    loop {
        let oversized = window.context.iter().find_map(|row| {
            (window
                .context
                .iter()
                .filter(|other| other.view.agent_id == row.view.agent_id)
                .count()
                > MAX_CHAT_CONTEXT_PER_AGENT)
                .then_some(row.view.agent_id)
        });
        let global: Vec<_> = window.context.iter().map(|row| &row.view).collect();
        if let Some(agent) = oversized {
            let first = window
                .context
                .iter()
                .position(|row| row.view.agent_id == agent)
                .expect("oversized agent exists");
            window.context.remove(first);
        } else if window.context.len() > MAX_CHAT_CONTEXT
            || array_bytes(&global)? > MAX_CHAT_CONTEXT_BYTES
        {
            window.context.remove(0);
        } else {
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn apply(
    tables: &mut Tables<'_>,
    agents: &RowTable<'_>,
    turns: &RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let mut window = windows(&tables.windows)?;
    match &entry.record {
        Record::ConversationDispatchUncertain {
            agent,
            turn,
            dispatch_intent_lsn,
            ..
        } => {
            let mut issue =
                get_row::<BrowserTurn>(&tables.issues, &turn.to_bytes())?.ok_or_else(invalid)?;
            if issue.agent != *agent
                || issue.goal.is_some()
                || issue.spawn.is_some()
                || issue.end.is_some()
                || issue.intents.last() != Some(dispatch_intent_lsn)
                || issue.uncertain_dispatches.contains(dispatch_intent_lsn)
            {
                return Err(invalid());
            }
            issue.uncertain_dispatches.push(*dispatch_intent_lsn);
            put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
        }
        Record::ConversationContextResetQueued {
            agent,
            text,
            browser: _,
            context_lsn,
            context_reset_turn,
            ..
        } => {
            let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            if current.conversation_context_loss != Some(*context_reset_turn)
                || current.goal.is_some()
                || current.mode != dudes_types::Mode::Managed
                || current.context_lsn != *context_lsn
                || text.is_empty()
                || text.len() > MAX_CHAT_TEXT_BYTES
                || text.trim_matches([' ', '\t', '\r', '\n']) != text
                || text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n'))
            {
                return Err(invalid());
            }
            let value = Note {
                agent: *agent,
                text: text.clone(),
                task: None,
                answer_to: None,
                destination: BrowserNoteDestination::ManagedConversation {
                    context_lsn: *context_lsn,
                },
                state: NoteState::Queued,
            };
            let pending = public_pending(lsn, &value).expect("queued reset input");
            if !admits_pending(&pending_rows(&tables.notes, agents)?, &pending)? {
                return Err(invalid());
            }
            tables.notes.insert(lsn.0, encoded(&value)?.as_slice())?;
        }
        Record::ConversationQuestion { agent, origin, .. } => {
            let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            let issue = get_row::<BrowserTurn>(&tables.issues, &origin.turn.to_bytes())?
                .ok_or_else(invalid)?;
            let executed =
                get_row::<super::TurnRow>(turns, &origin.turn.to_bytes())?.ok_or_else(invalid)?;
            let input = note(&tables.notes, issue.note_lsn)?;
            let expected_context = if executed.reissued {
                issue.spawn.ok_or_else(invalid)?
            } else {
                let BrowserNoteDestination::ManagedConversation { context_lsn } = input.destination
                else {
                    return Err(invalid());
                };
                context_lsn
            };
            if current.open_turn != Some(origin.turn)
                || issue.agent != *agent
                || issue.goal.is_some()
                || issue.end.is_some()
                || executed.agent != *agent
                || executed.goal.is_some()
                || executed.mode != dudes_types::Mode::Managed
                || executed.status != dudes_types::TurnStatus::Spawned
                || origin.context_lsn != expected_context
            {
                return Err(invalid());
            }
        }
        Record::ConversationAnswerQueued {
            agent,
            question_lsn,
            origin,
            text,
            task,
            authority,
            ..
        } => {
            let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            if current.question_lsn != Some(*question_lsn)
                || current.conversation_question != Some(*origin)
                || current.question.as_ref().is_none_or(|question| {
                    dudes_proto::conversation_answer_task(question, text) != *task
                })
                || matches!(authority, dudes_proto::ConversationAnswerAuthority::Browser { browser } if browser.question_lsn != *question_lsn)
                || dudes_proto::normalized_chat_text(text) != Some(text.as_str())
            {
                return Err(invalid());
            }
            let value = Note {
                agent: *agent,
                text: text.clone(),
                task: Some(task.clone()),
                answer_to: Some(*question_lsn),
                destination: BrowserNoteDestination::ManagedConversation {
                    context_lsn: origin.context_lsn,
                },
                state: NoteState::Queued,
            };
            let pending = public_pending(lsn, &value).expect("queued answer");
            if !admits_pending(&pending_rows(&tables.notes, agents)?, &pending)? {
                return Err(invalid());
            }
            tables.notes.insert(lsn.0, encoded(&value)?.as_slice())?;
        }
        Record::ConversationMessageQueued {
            agent,
            text,
            browser: _,
            context_lsn,
            ..
        } => {
            let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            if current.conversation_context_loss.is_some()
                || current.goal.is_some()
                || current.mode != dudes_types::Mode::Managed
                || current.context_lsn != *context_lsn
                || text.is_empty()
                || text.len() > MAX_CHAT_TEXT_BYTES
                || text.trim_matches([' ', '\t', '\r', '\n']) != text
                || text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n'))
            {
                return Err(invalid());
            }
            let value = Note {
                agent: *agent,
                text: text.clone(),
                task: None,
                answer_to: None,
                destination: BrowserNoteDestination::ManagedConversation {
                    context_lsn: *context_lsn,
                },
                state: NoteState::Queued,
            };
            let pending = public_pending(lsn, &value).expect("queued input");
            if !admits_pending(&pending_rows(&tables.notes, agents)?, &pending)? {
                return Err(invalid());
            }
            tables.notes.insert(lsn.0, encoded(&value)?.as_slice())?;
        }
        Record::ConversationTurnDecided {
            agent,
            note_lsn,
            context_lsn,
            decision,
            ..
        } => {
            let value = conversation_note(tables, agents, *agent, *note_lsn, *context_lsn)?;
            if matches!(decision, NoteAdmission::Issue { task } if task != value.task.as_ref().unwrap_or(&value.text))
            {
                return Err(invalid());
            }
            let mut decisions =
                get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.unwrap_or_default();
            decisions.conversation = Some((
                lsn,
                *note_lsn,
                *context_lsn,
                matches!(decision, NoteAdmission::Issue { .. }),
            ));
            put_row(&mut tables.decisions, &agent.to_bytes(), &decisions)?;
        }
        Record::ConversationTurnIssued {
            agent,
            turn,
            note_lsn,
            context_lsn,
            task,
            lsn_of_decision,
            ..
        } => {
            if get_row::<super::TurnRow>(turns, &turn.to_bytes())?.is_some() {
                return Err(invalid());
            }
            let value = conversation_note(tables, agents, *agent, *note_lsn, *context_lsn)?;
            let mut first = None;
            for pending in tables.notes.iter()? {
                let (position, encoded) = pending?;
                let queued: Note = serde_json::from_slice(encoded.value())
                    .map_err(|source| Error::Row { source })?;
                if queued.agent == *agent
                    && matches!(queued.state, NoteState::Queued)
                    && queued.destination == value.destination
                {
                    let priority = (queued.answer_to.is_none(), position.value());
                    if first.is_none_or(|current| priority < current) {
                        first = Some(priority);
                    }
                }
            }
            if first.map(|(_, position)| position) != Some(note_lsn.0) {
                return Err(invalid());
            }
            let decisions =
                get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.ok_or_else(invalid)?;
            let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            if value.task.as_ref().unwrap_or(&value.text) != task
                || current.open_turn.is_some()
                || decisions.conversation != Some((*lsn_of_decision, *note_lsn, *context_lsn, true))
            {
                return Err(invalid());
            }
            reserve(
                tables,
                agents,
                &mut window,
                lsn,
                *agent,
                *turn,
                None,
                *note_lsn,
                None,
            )?;
        }
        Record::BrowserNoteQueued {
            agent,
            text,
            browser: _,
            destination,
        } => {
            get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
            if matches!(
                destination,
                BrowserNoteDestination::ManagedConversation { .. }
            ) || text.is_empty()
                || text.len() > MAX_CHAT_TEXT_BYTES
            {
                return Err(invalid());
            }
            let value = Note {
                agent: *agent,
                text: text.clone(),
                task: None,
                answer_to: None,
                destination: destination.clone(),
                state: NoteState::Queued,
            };
            let pending = public_pending(lsn, &value).expect("queued note is pending");
            if !admits_pending(&pending_rows(&tables.notes, agents)?, &pending)? {
                return Err(invalid());
            }
            tables.notes.insert(lsn.0, encoded(&value)?.as_slice())?;
        }
        Record::LoopDecided {
            agent, decision, ..
        } => {
            if let Some(goal) =
                get_row::<AgentRow>(agents, &agent.to_bytes())?.and_then(|row| row.goal)
            {
                if matches!(decision, LoopDecision::Done)
                    && get_row::<Lsn>(&tables.completions, &goal.to_bytes())?.is_none()
                {
                    put_row(&mut tables.completions, &goal.to_bytes(), &lsn)?;
                }
                let mut decisions =
                    get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.unwrap_or_default();
                decisions.ordinary =
                    Some((lsn, goal, matches!(decision, LoopDecision::Continue { .. })));
                put_row(&mut tables.decisions, &agent.to_bytes(), &decisions)?;
            }
        }
        Record::NoteTurnDecided {
            agent,
            goal,
            note_lsn,
            completion_lsn,
            decision,
            ..
        }
        | Record::TaughtNoteTurnDecided {
            agent,
            goal,
            note_lsn,
            completion_lsn,
            decision,
            ..
        } => {
            let contract = if matches!(entry.record, Record::TaughtNoteTurnDecided { .. }) {
                NoteContract::Taught
            } else {
                NoteContract::Raw
            };
            if contract == NoteContract::Taught {
                let Record::TaughtNoteTurnDecided { gate_trace, .. } = &entry.record else {
                    unreachable!()
                };
                if !dudes_proto::valid_taught_decision(decision, gate_trace) {
                    return Err(invalid());
                }
            }
            let value = note(&tables.notes, *note_lsn)?;
            if value.agent != *agent
                || !matches!(value.state, NoteState::Queued)
                || value.destination != (BrowserNoteDestination::ManagedGoal { goal: *goal })
                || get_row::<Lsn>(&tables.completions, &goal.to_bytes())? != Some(*completion_lsn)
                || get_row::<AgentRow>(agents, &agent.to_bytes())?.and_then(|row| row.goal)
                    != Some(*goal)
                || matches!(decision, NoteAdmission::Issue { task } if match contract {
                    NoteContract::Raw => task != value.task.as_ref().unwrap_or(&value.text),
                    NoteContract::Taught => !contract.task_matches(&value.text, task),
                })
            {
                return Err(invalid());
            }
            let reason = match decision {
                NoteAdmission::Wait { .. } => Some(ChatReason::OpenQuestion),
                NoteAdmission::Hold { .. } => Some(ChatReason::Hold),
                NoteAdmission::Exhausted {
                    reason: dudes_proto::ExhaustedReason::Hops,
                } => Some(ChatReason::Hops),
                NoteAdmission::Exhausted { .. } => Some(ChatReason::Budget),
                NoteAdmission::Issue { .. } => None,
            };
            let mut decisions =
                get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.unwrap_or_default();
            decisions.note = Some(NoteDecision {
                lsn,
                goal: *goal,
                note_lsn: *note_lsn,
                completion_lsn: *completion_lsn,
                issued: matches!(decision, NoteAdmission::Issue { .. }),
                reason,
                contract,
            });
            put_row(&mut tables.decisions, &agent.to_bytes(), &decisions)?;
        }
        Record::NoteOnlyTurnIssued {
            agent,
            turn,
            goal,
            task,
            note_lsn,
            completion_lsn,
            lsn_of_decision,
            answers,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            agent,
            turn,
            goal,
            task,
            note_lsn,
            completion_lsn,
            lsn_of_decision,
            answers,
            ..
        } => {
            let contract = if matches!(entry.record, Record::TaughtNoteOnlyTurnIssued { .. }) {
                NoteContract::Taught
            } else {
                NoteContract::Raw
            };
            if contract == NoteContract::Taught
                && !matches!(
                    entry.record,
                    Record::TaughtNoteOnlyTurnIssued {
                        mode: dudes_types::Mode::Managed,
                        ..
                    }
                )
            {
                return Err(invalid());
            }
            if contract == NoteContract::Taught
                && (get_row::<super::TurnRow>(turns, &turn.to_bytes())?.is_some()
                    || get_row::<AgentRow>(agents, &agent.to_bytes())?
                        .is_some_and(|row| row.open_turn.is_some()))
            {
                return Err(invalid());
            }
            let decisions =
                get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.ok_or_else(invalid)?;
            if !answers.is_empty()
                || !decisions.note.is_some_and(|row| {
                    row.lsn == *lsn_of_decision
                        && row.goal == *goal
                        && row.note_lsn == *note_lsn
                        && row.completion_lsn == *completion_lsn
                        && row.issued
                        && row.contract == contract
                })
            {
                return Err(invalid());
            }
            let value = note(&tables.notes, *note_lsn)?;
            if match contract {
                NoteContract::Raw => task != value.task.as_ref().unwrap_or(&value.text),
                NoteContract::Taught => !contract.task_matches(&value.text, task),
            } {
                return Err(invalid());
            }
            reserve(
                tables,
                agents,
                &mut window,
                lsn,
                *agent,
                *turn,
                Some(*goal),
                *note_lsn,
                Some((*completion_lsn, contract)),
            )?;
        }
        Record::TurnIssuedWithBrowserNote {
            agent,
            turn,
            goal,
            note_lsn,
            lsn_of_decision,
            ..
        } => {
            let decisions =
                get_row::<Decisions>(&tables.decisions, &agent.to_bytes())?.ok_or_else(invalid)?;
            if decisions.ordinary != Some((*lsn_of_decision, *goal, true))
                || get_row::<Lsn>(&tables.completions, &goal.to_bytes())?.is_some()
            {
                return Err(invalid());
            }
            reserve(
                tables,
                agents,
                &mut window,
                lsn,
                *agent,
                *turn,
                Some(*goal),
                *note_lsn,
                None,
            )?;
        }
        Record::BrowserNoteCancelled {
            agent, note_lsn, ..
        } => {
            let mut value = note(&tables.notes, *note_lsn)?;
            if value.agent != *agent || !matches!(value.state, NoteState::Queued) {
                return Err(invalid());
            }
            value.state = NoteState::Cancelled;
            value.text.clear();
            tables
                .notes
                .insert(note_lsn.0, encoded(&value)?.as_slice())?;
        }
        Record::BrowserNoteDispatchIntent {
            agent,
            note_lsn,
            turn,
        } => {
            let mut issue = require_issue(tables, *agent, *note_lsn, *turn)?;
            if issue.cancellation.is_some() || issue.spawn.is_some() || issue.end.is_some() {
                return Err(invalid());
            }
            if issue.goal.is_none()
                && issue
                    .intents
                    .last()
                    .is_some_and(|prior| !issue.uncertain_dispatches.contains(prior))
            {
                return Err(invalid());
            }
            issue.intents.push(lsn);
            put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
        }
        Record::BrowserNoteIssuedCancelled {
            agent,
            note_lsn,
            turn,
            ..
        } => {
            let mut issue = require_issue(tables, *agent, *note_lsn, *turn)?;
            if issue.goal.is_none()
                || !issue.intents.is_empty()
                || issue.spawn.is_some()
                || issue.end.is_some()
                || issue.cancellation.is_some()
            {
                return Err(invalid());
            }
            issue.cancellation = Some(Cancellation::Clean(lsn));
            put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
            let mut value = note(&tables.notes, *note_lsn)?;
            value.state = NoteState::Cancelled;
            value.text.clear();
            tables
                .notes
                .insert(note_lsn.0, encoded(&value)?.as_slice())?;
            update_history(&mut window, *turn, |view| {
                view.state = ChatTurnState::Cancelled;
            });
        }
        Record::BrowserNoteReissueCancelled {
            agent,
            note_lsn,
            turn,
            dispatch_intent_lsn,
            ..
        } => {
            let mut issue = require_issue(tables, *agent, *note_lsn, *turn)?;
            if issue.goal.is_none()
                || !issue.intents.contains(dispatch_intent_lsn)
                || issue.spawn.is_some()
                || issue.end.is_some()
                || issue.cancellation.is_some()
            {
                return Err(invalid());
            }
            issue.cancellation = Some(Cancellation::Ambiguous {
                lsn,
                intent: *dispatch_intent_lsn,
            });
            put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
            let mut value = note(&tables.notes, *note_lsn)?;
            value.state = NoteState::ReissueCancelled(*turn);
            tables
                .notes
                .insert(note_lsn.0, encoded(&value)?.as_slice())?;
            update_history(&mut window, *turn, |view| {
                view.state = ChatTurnState::ReissueCancelled;
            });
        }
        Record::BrowserNoteContextEmitted { agent, note_lsn } => {
            let mut value = note(&tables.notes, *note_lsn)?;
            if value.agent != *agent
                || !matches!(value.state, NoteState::Queued)
                || value.destination != BrowserNoteDestination::AttachedContext
            {
                return Err(invalid());
            }
            value.state = NoteState::ContextEmitted;
            value.text.clear();
            tables
                .notes
                .insert(note_lsn.0, encoded(&value)?.as_slice())?;
            window.context.push(ContextHistory {
                emission_lsn: lsn,
                view: ChatContextView {
                    agent_id: *agent,
                    note_lsn: note_lsn.0.to_string(),
                    state: ChatContextState::SentAsContext,
                },
            });
        }
        Record::TurnSpawned {
            agent,
            turn,
            reissued,
            ..
        } => {
            if let Some(mut issue) = get_row::<BrowserTurn>(&tables.issues, &turn.to_bytes())? {
                if issue.agent != *agent
                    || issue.intents.is_empty()
                    || matches!(issue.cancellation, Some(Cancellation::Clean(_)))
                {
                    return Err(invalid());
                }
                issue.spawn = Some(lsn);
                put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
                let mut value = note(&tables.notes, issue.note_lsn)?;
                value.state = NoteState::Delivered;
                value.text.clear();
                tables
                    .notes
                    .insert(issue.note_lsn.0, encoded(&value)?.as_slice())?;
                update_history(&mut window, *turn, |view| {
                    view.state = ChatTurnState::Running;
                    view.reissued |= *reissued;
                });
            }
        }
        Record::TurnEnded {
            agent,
            turn,
            mode,
            last_message,
            final_reply,
            provenance,
            outcome,
            exit,
            ..
        } => {
            if final_reply.as_ref().is_some_and(|reply| {
                last_message.is_some()
                    || reply.text.len() > dudes_proto::MAX_CHAT_REPLY_BYTES
                    || *mode != dudes_types::Mode::Managed
                    || *provenance != dudes_types::Provenance::Observed
            }) {
                return Err(invalid());
            }
            if let Some(mut issue) = get_row::<BrowserTurn>(&tables.issues, &turn.to_bytes())? {
                if issue.agent != *agent
                    || *mode != dudes_types::Mode::Managed
                    || matches!(issue.cancellation, Some(Cancellation::Clean(_)))
                {
                    return Err(invalid());
                }
                issue.end = Some(lsn);
                put_row(&mut tables.issues, &turn.to_bytes(), &issue)?;
                let mut value = note(&tables.notes, issue.note_lsn)?;
                value.state = NoteState::Delivered;
                value.text.clear();
                tables
                    .notes
                    .insert(issue.note_lsn.0, encoded(&value)?.as_slice())?;
                update_history(&mut window, *turn, |view| {
                    view.state = if matches!(
                        outcome,
                        Some(
                            TurnOutcome::Failed { .. }
                                | TurnOutcome::NoResult
                                | TurnOutcome::Killed { .. }
                                | TurnOutcome::Vanished
                                | TurnOutcome::NotLaunched { .. }
                                | TurnOutcome::LaunchRefused { .. }
                        )
                    ) || exit.code.is_some_and(|code| code != 0)
                        || exit.signal.is_some()
                    {
                        ChatTurnState::Failed
                    } else {
                        ChatTurnState::Ended
                    };
                    view.reply = final_reply
                        .as_ref()
                        .map(|reply| reply.text.clone())
                        .or_else(|| last_message.clone());
                    view.truncated = final_reply.as_ref().map(|reply| {
                        reply.completeness == dudes_proto::ChatReplyCompleteness::Truncated
                    });
                    let reply_limit = if final_reply.is_some() {
                        dudes_proto::MAX_CHAT_REPLY_BYTES
                    } else {
                        MAX_CHAT_TEXT_BYTES
                    };
                    if let Some(reply) = view.reply.as_mut()
                        && reply.len() > reply_limit
                    {
                        let mut end = reply_limit;
                        while !reply.is_char_boundary(end) {
                            end -= 1;
                        }
                        reply.truncate(end);
                        view.truncated = Some(true);
                    }
                });
            }
        }
        Record::AgentRetired { id, .. } => {
            window.turns.retain(|row| row.view.agent_id != *id);
            window.context.retain(|row| row.view.agent_id != *id);
        }
        _ => return Ok(()),
    }
    for row in &mut window.turns {
        while array_bytes(std::slice::from_ref(&row.view))? > MAX_CHAT_TURN_BYTES_PER_AGENT {
            let Some(reply) = row.view.reply.as_mut().filter(|text| !text.is_empty()) else {
                break;
            };
            let mut bound = reply.len().saturating_sub(1024);
            while !reply.is_char_boundary(bound) {
                bound -= 1;
            }
            reply.truncate(bound);
            row.view.truncated = Some(true);
        }
    }
    trim_windows(&mut window)?;
    let mut bound = ChatView::empty();
    bound.pending = pending_rows(&tables.notes, agents)?;
    if let Record::AgentRetired { id, .. } = &entry.record {
        bound.pending.retain(|row| row.agent_id != *id);
    }
    loop {
        bound.turns = window.turns.iter().map(|row| row.view.clone()).collect();
        bound.context_sent = window.context.iter().map(|row| row.view.clone()).collect();
        if reserved_object_bytes(&bound)? <= MAX_CHAT_BYTES {
            break;
        }
        if !window.turns.is_empty() {
            window.turns.remove(0);
        } else if !window.context.is_empty() {
            window.context.remove(0);
        } else {
            return Err(invalid());
        }
    }
    if window.turns.is_empty() && window.context.is_empty() {
        tables.windows.remove("window")?;
    } else {
        tables
            .windows
            .insert("window", encoded(&window)?.as_slice())?;
    }
    Ok(())
}

fn conversation_note(
    tables: &Tables<'_>,
    agents: &RowTable<'_>,
    agent: AgentId,
    note_lsn: Lsn,
    context_lsn: Lsn,
) -> Result<Note> {
    let value = note(&tables.notes, note_lsn)?;
    let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
    if value.agent != agent
        || !matches!(value.state, NoteState::Queued)
        || value.destination != (BrowserNoteDestination::ManagedConversation { context_lsn })
        || current.context_lsn != context_lsn
        || current.goal.is_some()
        || current.mode != dudes_types::Mode::Managed
    {
        return Err(invalid());
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn reserve(
    tables: &mut Tables<'_>,
    agents: &RowTable<'_>,
    window: &mut Windows,
    lsn: Lsn,
    agent: AgentId,
    turn: TurnId,
    goal: Option<GoalId>,
    note_lsn: Lsn,
    completion: Option<(Lsn, NoteContract)>,
) -> Result<()> {
    let current = get_row::<AgentRow>(agents, &agent.to_bytes())?.ok_or_else(invalid)?;
    if current.goal != goal || current.mode != dudes_types::Mode::Managed {
        return Err(invalid());
    }
    let mut value = note(&tables.notes, note_lsn)?;
    if value.agent != agent
        || !matches!(value.state, NoteState::Queued)
        || value.destination
            != goal.map_or(
                BrowserNoteDestination::ManagedConversation {
                    context_lsn: current.context_lsn,
                },
                |goal| BrowserNoteDestination::ManagedGoal { goal },
            )
        || get_row::<BrowserTurn>(&tables.issues, &turn.to_bytes())?.is_some()
    {
        return Err(invalid());
    }
    value.state = NoteState::Reserved(turn);
    tables
        .notes
        .insert(note_lsn.0, encoded(&value)?.as_slice())?;
    put_row(
        &mut tables.issues,
        &turn.to_bytes(),
        &BrowserTurn {
            agent,
            goal,
            note_lsn,
            issue_lsn: lsn,
            note_only: completion.is_some(),
            completion_lsn: completion.map(|value| value.0),
            contract: completion.map(|value| value.1),
            intents: Vec::new(),
            uncertain_dispatches: Vec::new(),
            spawn: None,
            end: None,
            cancellation: None,
        },
    )?;
    let view = ChatTurnView {
        reissued: false,
        agent_id: agent,
        turn_id: turn,
        note_lsn: note_lsn.0.to_string(),
        text: value.text,
        state: ChatTurnState::Reserved,
        reply: None,
        truncated: None,
    };
    if array_bytes(std::slice::from_ref(&view))? <= MAX_CHAT_TURN_BYTES_PER_AGENT {
        window.turns.push(TurnHistory {
            issue_lsn: lsn,
            view,
        });
    }
    Ok(())
}

fn update_history(window: &mut Windows, turn: TurnId, change: impl FnOnce(&mut ChatTurnView)) {
    if let Some(row) = window.turns.iter_mut().find(|row| row.view.turn_id == turn) {
        change(&mut row.view);
    }
}

fn pending_reason(note: &Note, current: &AgentRow) -> Option<ChatReason> {
    let changed = match note.destination {
        BrowserNoteDestination::ManagedGoal { goal } => current.goal != Some(goal),
        BrowserNoteDestination::ManagedConversation { context_lsn } => {
            matches!(note.state, NoteState::Queued)
                && (current.context_lsn != context_lsn || current.goal.is_some())
        }
        BrowserNoteDestination::AttachedContext => false,
    };
    let mode_changed = match note.destination {
        BrowserNoteDestination::AttachedContext => current.mode != dudes_types::Mode::Attached,
        _ => current.mode == dudes_types::Mode::Attached,
    };
    if changed {
        Some(
            if matches!(
                note.destination,
                BrowserNoteDestination::ManagedConversation { .. }
            ) && current.goal.is_none()
            {
                ChatReason::ContextChanged
            } else {
                ChatReason::GoalChanged
            },
        )
    } else if mode_changed {
        Some(ChatReason::ModeChanged)
    } else if current.halted {
        Some(ChatReason::Halted)
    } else if current.paused {
        Some(ChatReason::Paused)
    } else if current.question.is_some() {
        Some(ChatReason::OpenQuestion)
    } else if current.automatic_hold.is_some() {
        Some(ChatReason::Hold)
    } else {
        None
    }
}

pub(super) fn read(
    notes: &impl ReadableTable<u64, &'static [u8]>,
    issues: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    window: &impl ReadableTable<&'static str, &'static [u8]>,
    agents: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    overlays: &BTreeMap<Lsn, ChatOverlay>,
) -> Result<ChatView> {
    let history = windows(window)?;
    let mut view = ChatView::empty();
    view.pending = pending_rows(notes, agents)?;
    view.turns = history.turns.into_iter().map(|row| row.view).collect();
    view.context_sent = history.context.into_iter().map(|row| row.view).collect();
    for pending in &mut view.pending {
        let lsn = Lsn(pending.note_lsn.parse().map_err(|_| invalid())?);
        let current =
            get_row::<AgentRow>(agents, &pending.agent_id.to_bytes())?.ok_or_else(invalid)?;
        let queued = note(notes, lsn)?;
        pending.reason = pending_reason(&queued, &current).or(pending.reason);
        if pending.turn_id.is_some()
            && matches!(
                queued.destination,
                BrowserNoteDestination::ManagedConversation { .. }
            )
        {
            pending.cancellable = false;
        }
        if let Some(turn) = pending.turn_id {
            let issue = get_row::<BrowserTurn>(issues, &turn.to_bytes())?.ok_or_else(invalid)?;
            if issue.spawn.is_none() && issue.end.is_none() && !issue.intents.is_empty() {
                pending.state = ChatPendingState::DeliveryUncertain;
                pending.reason = Some(ChatReason::DeliveryUncertain);
                pending.cancellable = issue.goal.is_some() && issue.cancellation.is_none();
            }
        }
        if let Some(overlay) = overlays.get(&lsn)
            && (pending.state != ChatPendingState::DeliveryUncertain || pending.cancellable)
        {
            pending.state = if overlay.uncertain {
                ChatPendingState::DeliveryUncertain
            } else if pending.turn_id.is_some() {
                ChatPendingState::Reserved
            } else {
                ChatPendingState::Queued
            };
            pending.cancellable &= !overlay.delivery_started;
            pending.reason = if overlay.uncertain {
                Some(ChatReason::DeliveryUncertain)
            } else {
                overlay.reason.or(pending
                    .reason
                    .filter(|reason| *reason != ChatReason::DeliveryUncertain))
            };
        }
    }
    view.pending.retain(|pending| {
        let lsn = Lsn(pending
            .note_lsn
            .parse()
            .expect("projection positions are canonical"));
        overlays.get(&lsn).is_none_or(|overlay| {
            !overlay.delivery_started
                || overlay.uncertain
                || (pending.state == ChatPendingState::DeliveryUncertain && !pending.cancellable)
        })
    });
    for turn in &mut view.turns {
        let issue =
            get_row::<BrowserTurn>(issues, &turn.turn_id.to_bytes())?.ok_or_else(invalid)?;
        if issue.cancellation.is_none()
            && issue.end.is_none()
            && overlays
                .get(&issue.note_lsn)
                .is_some_and(|overlay| overlay.delivery_started && !overlay.uncertain)
        {
            turn.state = ChatTurnState::Running;
        }
        if issue.spawn.is_none()
            && issue.end.is_none()
            && !issue.intents.is_empty()
            && issue.cancellation.is_none()
            && overlays
                .get(&issue.note_lsn)
                .is_none_or(|overlay| overlay.uncertain)
        {
            turn.state = ChatTurnState::DeliveryUncertain;
        }
    }
    while encoded(&view)?.len() + b",\"chat\":".len() > MAX_CHAT_BYTES {
        if !view.turns.is_empty() {
            view.turns.remove(0);
        } else if !view.context_sent.is_empty() {
            view.context_sent.remove(0);
        } else {
            return Err(invalid());
        }
    }
    Ok(view)
}

pub(super) fn clean_cancelled(
    issues: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    turn: TurnId,
) -> Result<bool> {
    Ok(get_row::<BrowserTurn>(issues, &turn.to_bytes())?
        .is_some_and(|issue| matches!(issue.cancellation, Some(Cancellation::Clean(_)))))
}

pub(super) fn export_notes(
    lines: &mut Vec<String>,
    table: &impl ReadableTable<u64, &'static [u8]>,
) -> Result<()> {
    for row in table.iter()? {
        let (key, value) = row?;
        let row: Note =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        lines.push(format!(
            "chat_notes {:016x} {}",
            key.value(),
            serde_json::to_string(&row).map_err(|source| Error::Row { source })?
        ));
    }
    Ok(())
}

pub(super) fn export(
    notes: &impl ReadableTable<u64, &'static [u8]>,
    issues: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    decisions: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    completions: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    windows: &impl ReadableTable<&'static str, &'static [u8]>,
    lines: &mut Vec<String>,
) -> Result<()> {
    export_notes(lines, notes)?;
    super::export_rows::<16, BrowserTurn>(lines, "chat_issues", issues)?;
    super::export_rows::<16, Decisions>(lines, "chat_decisions", decisions)?;
    super::export_rows::<16, Lsn>(lines, "chat_completions", completions)?;
    for entry in windows.iter()? {
        let (key, value) = entry?;
        let window: Windows =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        lines.push(format!(
            "chat_windows {} {}",
            super::hex(key.value().as_bytes()),
            serde_json::to_string(&window).map_err(|source| Error::Row { source })?
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use dudes_proto::{
        ChatPendingState, ChatPendingView, MAX_CHAT_PENDING, MAX_CHAT_PENDING_BYTES,
        MAX_CHAT_PENDING_PER_AGENT, MAX_CHAT_TEXT_BYTES,
    };
    use dudes_types::AgentId;

    use super::admits_pending;

    fn agent(n: u8) -> AgentId {
        AgentId::from_bytes([n; 16])
    }

    fn pending(owner: u8, lsn: u64, text: &str) -> ChatPendingView {
        ChatPendingView {
            agent_id: agent(owner),
            note_lsn: lsn.to_string(),
            goal_id: None,
            text: text.to_owned(),
            state: ChatPendingState::Queued,
            turn_id: None,
            cancellable: true,
            reason: None,
        }
    }

    /// Fill `rows` with `count` rows spread so that no agent reaches its own cap.
    fn spread(count: usize, text: &str) -> Vec<ChatPendingView> {
        (0..count)
            .map(|n| {
                pending(
                    u8::try_from(n / 10 + 1).unwrap(),
                    u64::try_from(n).unwrap(),
                    text,
                )
            })
            .collect()
    }

    #[test]
    fn empty_and_oversized_text_is_refused() {
        assert!(!admits_pending(&[], &pending(1, 1, "")).unwrap());
        let longest = "x".repeat(MAX_CHAT_TEXT_BYTES);
        assert!(admits_pending(&[], &pending(1, 1, &longest)).unwrap());
        let too_long = "x".repeat(MAX_CHAT_TEXT_BYTES + 1);
        assert!(!admits_pending(&[], &pending(1, 1, &too_long)).unwrap());
    }

    #[test]
    fn the_global_count_cap_refuses_one_more() {
        let rows = spread(MAX_CHAT_PENDING - 1, "note");
        assert!(admits_pending(&rows, &pending(200, 999, "note")).unwrap());
        let rows = spread(MAX_CHAT_PENDING, "note");
        assert!(!admits_pending(&rows, &pending(200, 999, "note")).unwrap());
    }

    #[test]
    fn the_per_agent_cap_refuses_one_more_for_that_agent_only() {
        let rows: Vec<_> = (0..MAX_CHAT_PENDING_PER_AGENT - 1)
            .map(|n| pending(1, u64::try_from(n).unwrap(), "note"))
            .collect();
        assert!(admits_pending(&rows, &pending(1, 99, "note")).unwrap());
        let mut full = rows;
        full.push(pending(1, 98, "note"));
        assert!(!admits_pending(&full, &pending(1, 99, "note")).unwrap());
        assert!(admits_pending(&full, &pending(2, 99, "note")).unwrap());
    }

    #[test]
    fn the_byte_cap_counts_the_worst_state_and_escaped_text() {
        // A quote escapes to two bytes, so this text costs far more than its length once encoded.
        let escaped = "\"".repeat(MAX_CHAT_TEXT_BYTES);
        let mut rows = Vec::new();
        let mut owner = 1;
        let mut lsn = 0;
        loop {
            if rows
                .iter()
                .filter(|row: &&ChatPendingView| row.agent_id == agent(owner))
                .count()
                == MAX_CHAT_PENDING_PER_AGENT
            {
                owner += 1;
            }
            let candidate = pending(owner, lsn, &escaped);
            if !admits_pending(&rows, &candidate).unwrap() {
                break;
            }
            rows.push(candidate);
            lsn += 1;
        }
        assert!(
            rows.len() < MAX_CHAT_PENDING,
            "the byte bound, not the count bound, stopped admission"
        );
        let reserved: usize = rows
            .iter()
            .map(|row| row.reserved_bytes().unwrap())
            .sum::<usize>()
            + 2
            + (rows.len() - 1);
        assert!(
            reserved <= MAX_CHAT_PENDING_BYTES,
            "every admitted row stays inside the bound at its worst state: {reserved}"
        );
        let next = pending(owner, lsn, &escaped);
        let next_cost = next.reserved_bytes().unwrap() + 1;
        assert!(
            reserved + next_cost > MAX_CHAT_PENDING_BYTES,
            "the refused row really did not fit"
        );
    }
}
