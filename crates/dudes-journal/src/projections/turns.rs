//! The `turns` table: one row per turn, from `TurnIssued` (managed) or from the `TurnEnded`
//! that mints it (attached), to `TurnEnded`.

use std::collections::BTreeMap;

use dudes_proto::{Entry, ExitStatus, Record, TurnOutcome};
use dudes_types::{
    AgentId, GoalId, Lsn, Mode, ModelSelection, Provenance, SessionId, Timestamp, TurnId,
    TurnStatus, Usage,
};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use super::{RowTable, get_row, put_row};
use crate::error::{Error, Result};

/// One turn as the projection knows it. `#[serde(default)]` for forward compatibility.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnRow {
    /// The turn.
    pub turn: TurnId,
    /// The agent.
    pub agent: AgentId,
    /// The goal it serves; `None` for an attached turn.
    pub goal: Option<GoalId>,
    /// Managed or attached.
    pub mode: Mode,
    /// Where it is in its life.
    pub status: TurnStatus,
    /// The lsn of the record that created the row: `TurnIssued`, or the attached `TurnEnded`.
    pub lsn_issued: Lsn,
    /// Latest physical spawn, independent of earlier attempts under the same turn.
    pub spawn_lsn: Option<Lsn>,
    /// Latest terminal source; an earlier end does not settle a later spawn.
    pub end_lsn: Option<Lsn>,
    /// Accepted confirmation source for the current physical attempt.
    pub confirmation_lsn: Option<Lsn>,
    /// The latest `TurnSpawned` entry's recorded time. Absent when no spawn was observed.
    pub spawned_at: Option<Timestamp>,
    /// The child's pid, once spawned.
    pub pid: Option<u32>,
    /// The child's start time, once spawned.
    pub start_time: Option<u64>,
    /// Immutable managed-launch grant paired with the latest spawn receipt.
    pub mcp_grant: Option<Lsn>,
    /// The vendor session, as last learned.
    pub session: Option<SessionId>,
    /// Whether this turn has a durable, accepted `SessionLearned`.
    pub session_confirmed: bool,
    /// The model the turn was issued under, as `TurnIssued` recorded it. `None`
    /// where the record carried none — a journal written before the field, or an attached turn,
    /// which has no `TurnIssued` to carry one.
    pub model_selection: Option<ModelSelection>,
    /// True when the spawn came from replay.
    pub reissued: bool,
    /// How the end was known, once ended.
    pub provenance: Option<Provenance>,
    /// How the process ended, once ended.
    pub exit: Option<ExitStatus>,
    /// What it consumed, when known. The aggregate, and the only column budget reads.
    pub usage: Option<Usage>,
    /// What the provider attributed to each model it named, as `TurnEnded` recorded it.
    /// Empty is "not reported"; no key is an effective model and no budget table
    /// folds this — the two live side by side so an audit can see both without either becoming
    /// the other.
    pub model_usage: BTreeMap<String, Usage>,
    /// When it ended, from the `TurnEnded` entry's `at`.
    pub ended_at: Option<Timestamp>,
    /// How it ended; `None` where the emitter wrote none.
    pub outcome: Option<TurnOutcome>,
}

impl Default for TurnRow {
    fn default() -> Self {
        Self {
            turn: TurnId::NIL,
            agent: AgentId::NIL,
            goal: None,
            mode: Mode::Managed,
            status: TurnStatus::Issued,
            lsn_issued: Lsn::ZERO,
            spawn_lsn: None,
            end_lsn: None,
            confirmation_lsn: None,
            spawned_at: None,
            pid: None,
            start_time: None,
            mcp_grant: None,
            session: None,
            session_confirmed: false,
            model_selection: None,
            reissued: false,
            provenance: None,
            exit: None,
            usage: None,
            model_usage: BTreeMap::new(),
            ended_at: None,
            outcome: None,
        }
    }
}

impl TurnRow {
    /// `Issued` or `Spawned`: the process may exist and nothing has closed the turn.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.status, TurnStatus::Issued | TurnStatus::Spawned)
    }
}

/// What one `TurnEnded` closed, as the `turns` table knows it once the end has been applied.
///
/// This type exists to turn an ordering rule into a compile error. `TurnEnded` carries no goal,
/// so `budget_by_goal` has to learn the goal from the turn's row — which means the budget tables
/// cannot be folded before `turns` has seen the same entry. Written as a comment, that ordering
/// survives exactly until someone reorders two lines in `apply_batch`. Written as a value that
/// only [`apply`] produces and only [`budget::apply`](super::budget::apply) consumes, the wrong
/// order does not compile.
///
/// `None` where the entry is not an end, or is one the table refused (an unknown managed turn):
/// nothing was closed, so nothing is charged.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EndedTurn {
    /// The agent whose ceiling the spend counts against.
    pub agent: AgentId,
    pub mode: Mode,
    /// The goal the turn served: `Some` for a managed turn, `None` for an attached one, whose
    /// row the end itself minted with no goal on it.
    pub goal: Option<GoalId>,
    /// What the end reported, when it reported anything.
    pub usage: Option<Usage>,
    /// The session the vendor confirmed for this turn, when it confirmed one.
    pub confirmed_session: Option<SessionId>,
    /// The vendor's running total for that session, when it reports one.
    pub session_usage: Option<Usage>,
}

/// Mutate one row in place; a record naming a turn the table does not know is logged and
/// skipped.
fn update(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    id: TurnId,
    record: &Record,
    f: impl FnOnce(&mut TurnRow),
) -> Result<()> {
    let key = id.to_bytes();
    let Some(mut row) = get_row::<TurnRow>(table, &key)? else {
        tracing::warn!(target: "journal", lsn = %lsn, turn = %id, kind = ?record.kind(), "projection.unknown_turn");
        return Ok(());
    };
    f(&mut row);
    put_row(table, &key, &row)
}

pub(super) fn count_for_agent(
    table: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    agent: AgentId,
) -> Result<u64> {
    let mut count = 0_u64;
    for entry in table.iter()? {
        let (_, value) = entry?;
        let row: TurnRow =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        if row.agent == agent {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Apply `entry` to the `turns` table. `entry.at` is read only where a row field needs it;
/// ordering is `lsn`'s job alone.
///
/// Returns the [`EndedTurn`] the budget tables charge, and `None` for every entry that closed
/// no turn. See [`EndedTurn`] for why the value is returned rather than re-read.
#[expect(
    clippy::match_same_arms,
    reason = "the `Checkpoint` arm is deliberately identical to the wildcard: it exists so the change that teaches something to read a checkpoint has to visit this fold. `expect` rather than `allow` so that giving the arm a body of its own reports the stale attribute instead of leaving it behind"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one arm per record kind this table folds, and the arms are the contract: splitting the match into per-record functions would move the exhaustiveness this function's shape provides. `expect` rather than `allow` so that a future shortening reports the stale attribute"
)]
pub(crate) fn apply(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<Option<EndedTurn>> {
    let record = &entry.record;
    match record {
        Record::ConversationTurnIssued {
            agent,
            turn,
            session,
            model_selection,
            ..
        } => {
            let row = TurnRow {
                turn: *turn,
                agent: *agent,
                goal: None,
                mode: Mode::Managed,
                status: TurnStatus::Issued,
                lsn_issued: lsn,
                session: session.clone(),
                model_selection: Some(model_selection.clone()),
                ..TurnRow::default()
            };
            put_row(table, &turn.to_bytes(), &row)?;
            Ok(None)
        }
        Record::TurnIssued {
            agent,
            turn,
            goal,
            session,
            mode,
            model_selection,
            ..
        } => {
            let row = TurnRow {
                turn: *turn,
                agent: *agent,
                goal: Some(*goal),
                mode: *mode,
                status: TurnStatus::Issued,
                lsn_issued: lsn,
                session: session.clone(),
                model_selection: model_selection.clone(),
                ..TurnRow::default()
            };
            put_row(table, &turn.to_bytes(), &row)?;
            Ok(None)
        }
        Record::NoteOnlyTurnIssued {
            agent,
            turn,
            goal,
            session,
            mode,
            model_selection,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            agent,
            turn,
            goal,
            session,
            mode,
            model_selection,
            ..
        }
        | Record::TurnIssuedWithBrowserNote {
            agent,
            turn,
            goal,
            session,
            mode,
            model_selection,
            ..
        } => {
            let row = TurnRow {
                turn: *turn,
                agent: *agent,
                goal: Some(*goal),
                mode: *mode,
                status: TurnStatus::Issued,
                lsn_issued: lsn,
                session: session.clone(),
                model_selection: Some(model_selection.clone()),
                ..TurnRow::default()
            };
            put_row(table, &turn.to_bytes(), &row)?;
            Ok(None)
        }
        Record::TurnSpawned {
            turn,
            pid,
            start_time,
            session,
            reissued,
            mcp_grant,
            ..
        } => {
            update(table, lsn, *turn, record, |t| {
                t.status = TurnStatus::Spawned;
                t.spawned_at = Some(entry.at);
                t.spawn_lsn = Some(lsn);
                t.pid = Some(*pid);
                t.start_time = Some(*start_time);
                t.reissued = *reissued;
                t.mcp_grant = *mcp_grant;
                if session.is_some() {
                    t.session.clone_from(session);
                }
                // A confirmation before a reissue carrier belongs to an earlier launch
                // attempt. The current attempt's confirmation follows this record.
                if *reissued {
                    t.session_confirmed = false;
                    t.confirmation_lsn = None;
                }
            })?;
            Ok(None)
        }
        Record::SessionLearned { turn, session, .. } => {
            update(table, lsn, *turn, record, |t| {
                t.session = Some(session.clone());
                t.session_confirmed = true;
                t.confirmation_lsn = Some(lsn);
            })?;
            Ok(None)
        }
        Record::TurnEnded {
            agent,
            turn,
            mode,
            provenance,
            exit,
            usage,
            model_usage,
            session,
            session_usage,
            outcome,
            ..
        } => {
            let key = turn.to_bytes();
            let mut row = match get_row::<TurnRow>(table, &key)? {
                Some(row) => row,
                // An attached turn has no `TurnIssued`: the end mints it.
                None if *mode == Mode::Attached => TurnRow {
                    turn: *turn,
                    agent: *agent,
                    mode: Mode::Attached,
                    lsn_issued: lsn,
                    ..TurnRow::default()
                },
                None => {
                    tracing::warn!(target: "journal", lsn = %lsn, turn = %turn, kind = ?record.kind(), "projection.unknown_turn");
                    return Ok(None);
                }
            };
            // The outcome is what makes `Killed` reachable. `Vanished`
            // is an end the reaper observed, not a kill, so it stays `Ended`.
            row.status = if matches!(outcome, Some(TurnOutcome::Killed { .. })) {
                TurnStatus::Killed
            } else {
                TurnStatus::Ended
            };
            row.provenance = Some(*provenance);
            row.exit = Some(*exit);
            row.usage = *usage;
            row.model_usage.clone_from(model_usage);
            row.ended_at = Some(entry.at);
            row.end_lsn = Some(lsn);
            row.outcome.clone_from(outcome);
            if session.is_some() {
                row.session.clone_from(session);
            }
            // Written unconditionally: a second `TurnEnded` on the same turn is last-write-wins
            // by `lsn`, and `apply_batch` guarantees the order. No v1
            // emitter produces one; `second_turn_ended_rewrites_the_row_last_write_wins_by_lsn`
            // states the rule.
            put_row(table, &key, &row)?;
            // The goal comes off the row rather than the record, which names none. For a managed
            // turn that is the goal `TurnIssued` recorded; for an attached one the row was just
            // minted without a goal, and the budget arm attributes it.
            Ok(Some(EndedTurn {
                mode: row.mode,
                agent: *agent,
                goal: row.goal,
                usage: row.usage,
                confirmed_session: row.session.clone().filter(|_| row.session_confirmed),
                session_usage: *session_usage,
            }))
        }
        // `KillDeferred` deliberately moves no row: it records that a standing kill was *not*
        // carried out, which changes neither the turn's status nor its end.
        // It is journaled for `dudes why`. The test named after it pins that it is inert *and*
        // that `last_applied_lsn` still advances past it, so a later edit to this arm cannot
        // give it meaning unnoticed.
        // Named, not left to the wildcard: see the note on the same arm in `agents.rs`.
        Record::Checkpoint { .. } => Ok(None),
        _ => Ok(None),
    }
}
