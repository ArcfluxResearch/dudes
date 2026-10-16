//! The `agents` table: one row per hired agent, deleted on retire.

use std::path::PathBuf;

use dudes_proto::record::RecordedWorkspace;
use dudes_proto::{Entry, LiveActivityKind, LoopDecision, Record, TurnOutcome};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, GoalId, GoalStatus, Lsn, Mode, Provenance, Provider,
    SessionId, Timestamp, TurnId, Usd,
};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use super::turns::TurnRow;
use super::{GoalRow, RowTable, get_row, put_row};
use crate::error::Result;

/// One agent as the projection knows it. `#[serde(default)]`: a row written by an older
/// build without a later-added field still reads. `spent_today` is not here yet — it belongs
/// to the budget projections, with the rest of the day-bucketed views.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentRow {
    /// The agent.
    pub id: AgentId,
    /// Display name.
    pub name: String,
    /// Authored cast character. Absence preserves the automatic appearance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent colour. Absence preserves the neutral accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<dudes_types::AccentColour>,
    /// Recorded hire time.
    pub hired_at: Timestamp,
    /// Actual immutable hire position.
    pub hired_lsn: Lsn,
    /// Role text.
    pub role: String,
    /// Newest appearance edit, absent before the first edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look_lsn: Option<Lsn>,
    /// Department.
    pub department: String,
    /// Which CLI drives it.
    pub provider: Provider,
    /// The repository it works in.
    pub cwd: PathBuf,
    /// Immutable enrolled identity and label copied from the hire record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<RecordedWorkspace>,
    /// Where it is in its life.
    pub status: AgentStatus,
    /// Latest journaled fixed activity, separate from lifecycle status.
    pub live_activity: Option<LiveActivityRow>,
    /// `Managed` or `Attached`.
    pub mode: Mode,
    /// `loop.pause` standing (a control flag, read at gate 1).
    pub paused: bool,
    /// `loop.halt` standing.
    pub halted: bool,
    /// Rung after the last `BreakerBeat`.
    pub breaker: BreakerRung,
    /// The goal it pursues.
    pub goal: Option<GoalId>,
    /// Latest context generation, initially the hire position.
    pub context_lsn: Lsn,
    /// Exact failed finite resume awaiting explicit context-reset acknowledgement.
    pub conversation_context_loss: Option<TurnId>,
    /// The vendor-confirmed session for this agent: the id the vendor named back to us, and the
    /// only one a later turn may resume. `SessionLearned` is its only establishing
    /// writer; a session-invalidating record and a replayed launch refusal may clear it. Announcing an
    /// id the daemon minted does not confirm it, so this stays `None` until the vendor speaks.
    /// The id a turn *announced* is `TurnRow::session`, which is not this and is not resumable.
    pub session: Option<SessionId>,
    /// Actual confirmation that established the current session.
    pub session_lsn: Option<Lsn>,
    /// The open human question, if any.
    pub question: Option<String>,
    /// Where the `HumanQuestion` that opened [`AgentRow::question`] sits in the journal — the
    /// question's identity. Set by that record alone and cleared with the question; a
    /// `LoopDecided{Wait}` repeats the text and never sets a position.
    ///
    /// Omitted rather than written as `null` while no question is open, so every position a row
    /// stores is a number that names a frame in its own journal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_lsn: Option<Lsn>,
    /// Exact finite origin of the current structured question.
    pub conversation_question: Option<dudes_proto::ConversationQuestionOrigin>,
    /// The agent's control identity: where its newest `Pause`, `Resume`, `Halt` or `AgentHeld` sits in the
    /// journal. Set by those records alone, whatever their attribution, and never cleared.
    /// Omitted rather than written as `null` while none has been journaled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_lsn: Option<Lsn>,
    /// Standing automatic cause, cleared by Resume or a new goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_hold: Option<dudes_proto::AutomaticHold>,
    /// Managed turns issued over its life.
    pub turns: u64,
    /// The managed turn that is `Issued` or `Spawned`, if one is.
    pub open_turn: Option<TurnId>,
    /// When this agent's last turn ended, from that entry's `at`.
    pub last_turn_ended_at: Option<Timestamp>,
    /// How that end was known: the provenance of the highest-lsn `TurnEnded` for this agent.
    /// Gate 2's input — an `Inferred` end may not yield `Done` (invariant 3).
    pub last_turn_end: Option<Provenance>,
    /// The vendor-run turn cap recorded at hire, or `None` for no cap. Written once
    /// by the `AgentHired` arm and never updated: v1 has no method that changes it, and a row
    /// projected from a journal written before the field reads `None` under `#[serde(default)]`.
    pub max_turns: Option<u32>,
    /// The vendor-run budget backstop recorded at hire, or `None` for none, with the
    /// lifetime [`AgentRow::max_turns`] describes. Not a spend ceiling: the day-bucketed budget
    /// views, not this, are what gate 5 admits against.
    pub max_budget: Option<Usd>,
    /// The lease the agent was hired into, when it was hired into a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<dudes_types::WorktreeId>,
}

/// A journal-derived kind and time, before a reader computes its remaining lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveActivityRow {
    /// Fixed content-free kind.
    pub kind: LiveActivityKind,
    /// The journal entry time.
    pub at: Timestamp,
}

impl Default for AgentRow {
    fn default() -> Self {
        Self {
            id: AgentId::NIL,
            name: String::new(),
            character: None,
            accent: None,
            hired_at: Timestamp::UNIX_EPOCH,
            hired_lsn: Lsn::ZERO,
            role: String::new(),
            look_lsn: None,
            department: String::new(),
            provider: Provider::Claude,
            cwd: PathBuf::new(),
            workspace: None,
            status: AgentStatus::Idle,
            live_activity: None,
            mode: Mode::Managed,
            paused: false,
            halted: false,
            breaker: BreakerRung::Healthy,
            goal: None,
            context_lsn: Lsn::ZERO,
            conversation_context_loss: None,
            session: None,
            session_lsn: None,
            question: None,
            question_lsn: None,
            conversation_question: None,
            control_lsn: None,
            automatic_hold: None,
            turns: 0,
            open_turn: None,
            last_turn_ended_at: None,
            last_turn_end: None,
            max_turns: None,
            max_budget: None,
            worktree: None,
        }
    }
}

impl AgentRow {
    fn settle(&mut self, goal_completed: bool) {
        self.status = if self.mode == Mode::Attached {
            AgentStatus::Attached
        } else if self.question.is_some() {
            AgentStatus::Waiting
        } else if self.open_turn.is_some() {
            AgentStatus::Running
        } else if goal_completed {
            AgentStatus::Done
        } else {
            AgentStatus::Idle
        };
    }
}

/// Mutate one row in place; a record naming an agent the table does not know is logged and
/// skipped, never a panic.
fn update(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    id: AgentId,
    record: &Record,
    f: impl FnOnce(&mut AgentRow),
) -> Result<()> {
    let key = id.to_bytes();
    let Some(mut row) = get_row::<AgentRow>(table, &key)? else {
        tracing::warn!(target: "journal", lsn = %lsn, agent = %id, kind = ?record.kind(), "projection.unknown_agent");
        return Ok(());
    };
    f(&mut row);
    put_row(table, &key, &row)
}

/// Apply `entry` to the `agents` table. `entry.at` is read only where a row field needs it;
/// ordering is `lsn`'s job alone.
pub(crate) fn apply(
    table: &mut RowTable<'_>,
    turns: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    goals: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let record = &entry.record;
    if let Some((agent, goal)) = record.goal_install() {
        return update(table, lsn, agent, record, |a| {
            a.goal = Some(goal.id);
            a.context_lsn = lsn;
            a.automatic_hold = None;
            a.live_activity = None;
        });
    }
    match record {
        Record::AgentHired {
            id,
            name,
            role,
            department,
            provider,
            character,
            accent,
            cwd,
            workspace,
            max_turns,
            max_budget,
            worktree,
            ..
        } => {
            let row = AgentRow {
                id: *id,
                name: name.clone(),
                hired_at: entry.at,
                hired_lsn: lsn,
                context_lsn: lsn,
                role: role.clone(),
                look_lsn: None,
                department: department.clone(),
                provider: *provider,
                character: *character,
                accent: *accent,
                cwd: cwd.clone(),
                workspace: workspace.clone(),
                max_turns: *max_turns,
                max_budget: *max_budget,
                worktree: *worktree,
                ..AgentRow::default()
            };
            put_row(table, &id.to_bytes(), &row)
        }
        Record::AgentRenamed { id, name, .. } => {
            update(table, lsn, *id, record, |row| row.name.clone_from(name))
        }
        Record::AgentLookChanged {
            id,
            role,
            character,
            accent,
            ..
        } => update(table, lsn, *id, record, |row| {
            row.role.clone_from(role);
            row.character = *character;
            row.accent = *accent;
            row.look_lsn = Some(lsn);
        }),
        Record::AgentRetired { id, .. } => {
            if table.remove(&id.to_bytes())?.is_none() {
                tracing::warn!(target: "journal", lsn = %lsn, agent = %id, "projection.unknown_agent");
            }
            Ok(())
        }
        Record::AgentHeld { agent, cause, .. } => update(table, lsn, *agent, record, |a| {
            a.automatic_hold = Some(cause.clone());
            a.control_lsn = Some(lsn);
            a.live_activity = Some(LiveActivityRow {
                kind: LiveActivityKind::Idle,
                at: entry.at,
            });
        }),
        Record::GoalCleared { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.goal = None;
            a.context_lsn = lsn;
        }),
        Record::GoalReassigned { goal, from, to, .. } => {
            update(table, lsn, *from, record, |a| {
                a.goal = None;
                a.context_lsn = lsn;
            })?;
            update(table, lsn, *to, record, |a| {
                a.goal = Some(*goal);
                a.context_lsn = lsn;
            })
        }
        _ => apply_flow(table, turns, goals, lsn, entry),
    }
}

fn completed_goal_for_settlement(
    table: &RowTable<'_>,
    goals: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    record: &Record,
) -> Result<bool> {
    Ok(match record {
        Record::TurnEnded { agent, .. }
        | Record::ConversationQuestion { agent, .. }
        | Record::ConversationAnswerQueued { agent, .. }
        | Record::HumanQuestion { agent, .. }
        | Record::HumanAnswer { agent, .. }
        | Record::Detached { agent, .. }
        | Record::BrowserNoteIssuedCancelled { agent, .. }
        | Record::NoteOnlyTurnIssued { agent, .. }
        | Record::TaughtNoteOnlyTurnIssued { agent, .. }
        | Record::TurnIssuedWithBrowserNote { agent, .. } => {
            let agent = get_row::<AgentRow>(table, &agent.to_bytes())?;
            match agent.and_then(|row| row.goal) {
                Some(goal) => get_row::<GoalRow>(goals, &goal.to_bytes())?
                    .is_some_and(|row| row.status == GoalStatus::Achieved),
                None => false,
            }
        }
        _ => false,
    })
}

/// The records that move an agent through its life: turns, decisions, questions,
/// attach/detach, control flags and the breaker.
#[expect(
    clippy::match_same_arms,
    reason = "the `Checkpoint` arm is deliberately identical to the wildcard: it exists so the change that teaches something to read a checkpoint has to visit this fold. `expect` rather than `allow` so that giving the arm a body of its own reports the stale attribute instead of leaving it behind"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered lifecycle fold keeps each record's status and activity changes together"
)]
fn apply_flow(
    table: &mut RowTable<'_>,
    turns: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    goals: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let record = &entry.record;
    let goal_completed = completed_goal_for_settlement(table, goals, record)?;
    match record {
        // `TurnIssued.session` is what this turn *announced*; it moves `TurnRow::session` and
        // nothing here. `open_turn` is what must
        // survive: it is what stops a second turn being issued while one is open.
        Record::TurnIssued { agent, turn, .. }
        | Record::ConversationTurnIssued { agent, turn, .. }
        | Record::NoteOnlyTurnIssued { agent, turn, .. }
        | Record::TaughtNoteOnlyTurnIssued { agent, turn, .. }
        | Record::TurnIssuedWithBrowserNote { agent, turn, .. } => {
            update(table, lsn, *agent, record, |a| {
                a.turns += 1;
                a.open_turn = Some(*turn);
                a.settle(goal_completed);
            })
        }
        Record::TurnSpawned {
            agent,
            turn,
            reissued,
            ..
        } => {
            let finite = get_row::<TurnRow>(turns, &turn.to_bytes())?
                .is_some_and(|row| row.mode == Mode::Managed && row.goal.is_none());
            update(table, lsn, *agent, record, |a| {
                if *reissued {
                    a.session = None;
                    a.session_lsn = None;
                    if finite {
                        a.context_lsn = lsn;
                    }
                }
                if a.open_turn == Some(*turn) {
                    a.live_activity = Some(LiveActivityRow {
                        kind: LiveActivityKind::Working,
                        at: entry.at,
                    });
                }
            })
        }
        Record::LiveActivityObserved { agent, kind } => update(table, lsn, *agent, record, |a| {
            if a.question.is_none() && !a.halted && !a.paused {
                a.live_activity = Some(LiveActivityRow {
                    kind: *kind,
                    at: entry.at,
                });
            }
        }),
        Record::SessionLearned { agent, session, .. } => update(table, lsn, *agent, record, |a| {
            a.session = Some(session.clone());
            a.session_lsn = Some(lsn);
            a.conversation_context_loss = None;
        }),
        // `TurnEnded.session` is the id the turn ran on, and repeating an id is not the vendor
        // confirming it: it moves `TurnRow::session` and nothing here, for the same reason the
        // `TurnIssued` arm above writes none.
        Record::TurnEnded {
            agent,
            turn,
            mode,
            provenance,
            outcome,
            ..
        } => {
            let agent_session = get_row::<AgentRow>(table, &agent.to_bytes())?
                .and_then(|agent_row| agent_row.session);
            let invalidates_resume = *mode == Mode::Managed
                && qualifying_unconfirmed_resume_end(outcome.as_ref())
                && get_row::<TurnRow>(turns, &turn.to_bytes())?.is_some_and(|turn_row| {
                    !turn_row.session_confirmed
                        && turn_row
                            .session
                            .as_ref()
                            .is_some_and(|announced| agent_session.as_ref() == Some(announced))
                });
            let finite_loss = invalidates_resume
                && get_row::<TurnRow>(turns, &turn.to_bytes())?
                    .is_some_and(|row| row.mode == Mode::Managed && row.goal.is_none());
            update(table, lsn, *agent, record, |a| {
                if *mode == Mode::Managed && a.open_turn == Some(*turn) {
                    a.open_turn = None;
                }
                // Written unconditionally, so "the highest-lsn `TurnEnded` for this agent" falls
                // out of the lsn order `apply_batch` enforces.
                a.last_turn_ended_at = Some(entry.at);
                a.live_activity = Some(LiveActivityRow {
                    kind: if a.question.is_some() {
                        LiveActivityKind::Waiting
                    } else {
                        LiveActivityKind::Idle
                    },
                    at: entry.at,
                });
                a.last_turn_end = Some(*provenance);
                if invalidates_resume || matches!(outcome, Some(TurnOutcome::LaunchRefused { .. }))
                {
                    a.session = None;
                    a.session_lsn = None;
                }
                if finite_loss {
                    a.context_lsn = lsn;
                    a.conversation_context_loss = Some(*turn);
                }
                a.settle(goal_completed);
            })
        }
        Record::ConversationContextResetQueued { agent, .. } => {
            update(table, lsn, *agent, record, |a| {
                a.conversation_context_loss = None;
            })
        }
        Record::BrowserNoteIssuedCancelled { agent, turn, .. } => {
            update(table, lsn, *agent, record, |a| {
                if a.open_turn == Some(*turn) {
                    a.open_turn = None;
                }
                a.settle(goal_completed);
            })
        }
        Record::ConversationTurnDecided {
            agent, decision, ..
        }
        | Record::NoteTurnDecided {
            agent, decision, ..
        }
        | Record::TaughtNoteTurnDecided {
            agent, decision, ..
        } => update(table, lsn, *agent, record, |a| match decision {
            dudes_proto::NoteAdmission::Wait { .. } => a.status = AgentStatus::Waiting,
            dudes_proto::NoteAdmission::Exhausted { .. } => a.status = AgentStatus::Exhausted,
            dudes_proto::NoteAdmission::Issue { .. } | dudes_proto::NoteAdmission::Hold { .. } => {}
        }),
        Record::MailWakeTurnDecided(decision) => update(table, lsn, decision.agent, record, |a| {
            match decision.decision {
                dudes_proto::MailWakeAdmission::Wait { .. } => a.status = AgentStatus::Waiting,
                dudes_proto::MailWakeAdmission::Exhausted { .. } => {
                    a.status = AgentStatus::Exhausted;
                }
                dudes_proto::MailWakeAdmission::Issue { .. }
                | dudes_proto::MailWakeAdmission::Hold { .. } => {}
            }
        }),
        Record::LoopDecided {
            agent, decision, ..
        } => update(table, lsn, *agent, record, |a| match decision {
            LoopDecision::Wait { question } => {
                a.question = Some(question.clone());
                a.status = AgentStatus::Waiting;
            }
            LoopDecision::Done => a.status = AgentStatus::Done,
            LoopDecision::Exhausted { .. } => a.status = AgentStatus::Exhausted,
            LoopDecision::Continue { .. } | LoopDecision::Hold { .. } => {}
        }),
        // Settled, not set: "Waiting" is an open question on an agent that is not attached, so a
        // human holding the keyboard still reads `Attached` while the question is kept.
        Record::ConversationQuestion { agent, text, .. }
        | Record::HumanQuestion { agent, text } => update(table, lsn, *agent, record, |a| {
            a.question = Some(text.clone());
            a.question_lsn = Some(lsn);
            a.conversation_question = match record {
                Record::ConversationQuestion { origin, .. } => Some(*origin),
                _ => None,
            };
            a.live_activity = Some(LiveActivityRow {
                kind: LiveActivityKind::Waiting,
                at: entry.at,
            });
            a.settle(goal_completed);
        }),
        Record::ConversationAnswerQueued { agent, .. } | Record::HumanAnswer { agent, .. } => {
            update(table, lsn, *agent, record, |a| {
                a.question = None;
                a.question_lsn = None;
                a.conversation_question = None;
                a.live_activity = None;
                a.settle(goal_completed);
            })
        }
        Record::Attached { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.mode = Mode::Attached;
            a.status = AgentStatus::Attached;
        }),
        Record::Detached { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.mode = Mode::Managed;
            a.settle(goal_completed);
        }),
        Record::Pause { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.paused = true;
            a.control_lsn = Some(lsn);
            a.live_activity = None;
        }),
        // `Resume` lifts *both* standing causes, because it is the only record that lifts
        // either: `machine.rs` routes `Command::Resume` through `Then::ClearedCause`, the
        // supervisor's own fold clears the pair, and no other record and no wire method
        // un-halts an agent. Clearing `paused` alone made `halted` a one-way latch here, so
        // this projection and the supervisor's answered differently about the same agent —
        // the journal is the authority.
        Record::Resume { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.automatic_hold = None;
            a.halted = false;
            a.paused = false;
            a.control_lsn = Some(lsn);
            a.live_activity = None;
        }),
        Record::Halt { agent, .. } => update(table, lsn, *agent, record, |a| {
            a.halted = true;
            a.control_lsn = Some(lsn);
            a.live_activity = None;
        }),
        Record::BreakerBeat {
            agent, rung_after, ..
        } => update(table, lsn, *agent, record, |a| a.breaker = *rung_after),
        // `KillDeferred` deliberately moves no row: a kill that was *not* sent leaves the agent
        // exactly where it was. See the `turns` arm for the same note.
        //
        // `Checkpoint` is named rather than left to the wildcard because it is the one record
        // the *writer* produces, and the change that teaches anything to read one has to visit
        // every fold. A wildcard is not a visit; this arm is (`Writer::emit_checkpoint`).
        Record::Checkpoint { .. } => Ok(()),
        _ => Ok(()),
    }
}

fn qualifying_unconfirmed_resume_end(outcome: Option<&TurnOutcome>) -> bool {
    matches!(
        outcome,
        Some(
            TurnOutcome::Completed
                | TurnOutcome::MaxTurns
                | TurnOutcome::BudgetExhausted
                | TurnOutcome::Failed { .. }
                | TurnOutcome::NoResult
        )
    )
}
