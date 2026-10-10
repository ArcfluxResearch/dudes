//! The `goals` table: one row per `GoalSet`, from the goal's minting to whatever ends it.
//!
//! The row holds the goal's definition, its owner, where it is in its life, the hop count gate 6
//! reads and the latest predicate verdict. It deliberately holds **no spend**: `goals`
//! and `budget_by_goal` are two tables, and `dudes_proto::GoalView` assembles a goal and
//! its money at read time, so there is exactly one place a goal's spend is stored.

use dudes_proto::{Entry, GoalBudget, LoopDecision, Predicate, PredicateOutcome, Record};
use dudes_types::{AgentId, GoalId, GoalStatus, Lsn, Mode, Timestamp, Usd};
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};

use super::{AgentRow, RowTable, get_row, put_row};
use crate::error::Result;

/// The first `LoopDecided { Done }` that carried a goal's observed pass: the
/// position and the daemon's recorded wall-clock label of that record. Zone-independent on purpose,
/// so a read can apply any day window to it and no persisted fact depends on the host zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstDone {
    /// The position of the Done decision, which is what makes the first one win.
    pub lsn: Lsn,
    /// `Entry.at` of that decision. A label, not an order: the position orders.
    pub at: Timestamp,
}

/// The first-completion index: ordered facts, then unresolved Done decisions after them.
pub(crate) type FirstDoneTable<'txn> = Table<'txn, &'static [u8; 25], &'static [u8]>;

const FACT: u8 = 0;
const FAULT: u8 = 1;

/// The key of a fact: `at` as offset-binary big-endian, so redb key order is time order across
/// negative and positive instants, then the goal. A window is a range read over these keys.
pub(super) fn fact_key(at: Timestamp, goal: GoalId) -> [u8; 25] {
    let mut key = [FACT; 25];
    key[1..9].copy_from_slice(&(at.unix_millis().cast_unsigned() ^ (1 << 63)).to_be_bytes());
    key[9..].copy_from_slice(&goal.to_bytes());
    key
}

/// The key of a Done decision that could not be resolved to its goal's own observed pass. Any such
/// key makes the aggregate unavailable rather than silently low, and each names the record at fault.
fn fault_key(lsn: Lsn) -> [u8; 25] {
    let mut key = [FAULT; 25];
    key[1..9].copy_from_slice(&lsn.0.to_be_bytes());
    key[9..].fill(0);
    key
}

/// Whether any unresolved Done decision has been folded.
pub(super) fn has_fault(
    table: &impl ReadableTable<&'static [u8; 25], &'static [u8]>,
) -> Result<bool> {
    Ok(table
        .range::<&[u8; 25]>(&fault_key(Lsn(0))..)?
        .next()
        .transpose()?
        .is_some())
}

/// One goal as the projection knows it. `#[serde(default)]` for forward compatibility, like the
/// other rows: a row written by an older build without a later-added field still reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalRow {
    /// The goal.
    pub id: GoalId,
    /// Who pursues it now; moved by `GoalReassigned`, never by anything else.
    pub agent: AgentId,
    /// The goal as the model reads it.
    pub text: String,
    /// How the daemon decides it is met.
    pub predicate: Predicate,
    /// Recorded configured check summary.
    pub completion_check: Option<dudes_proto::CompletionCheckRef>,
    /// What goal set saw of the check before the first turn.
    pub baseline: Option<dudes_proto::CheckBaseline>,
    /// Its ceiling, for the whole goal.
    pub budget: GoalBudget,
    /// Most managed turns it may consume (gate 6).
    pub hop_cap: u32,
    /// Where it is in its life.
    pub status: GoalStatus,
    /// Managed `TurnIssued` records for this goal. Goal-scoped, so
    /// it carries over a `GoalReassigned` — the row moves owner, the counter does not reset.
    pub turns_on_goal: u32,
    /// The latest `PredicateResult`, when one has run.
    pub last_result: Option<PredicateOutcome>,
    /// The first observed completion. Set once and never rewritten: reassignment,
    /// retirement, a replacement goal and a later result leave it alone.
    pub first_done: Option<FirstDone>,
}

impl Default for GoalRow {
    fn default() -> Self {
        Self {
            id: GoalId::NIL,
            agent: AgentId::NIL,
            text: String::new(),
            // Never observed: a row is only ever written from a `GoalSet`, which carries a real
            // predicate. `Predicate` has no `Default` of its own, and inventing one in the
            // contract crate to serve a `serde(default)` here would be the tail wagging the dog.
            predicate: Predicate::Command {
                argv: Vec::new(),
                timeout_s: 0,
            },
            // `Some(Usd::ZERO)` and **not** `None`, now that the two mean different things.
            // This default is never observed, so what it really encodes is
            // which way it fails if it ever were: a ceiling of nothing refuses the first turn and
            // is loud, while absence caps nothing and would be silent. The placeholder keeps the
            // refusing reading on purpose. It is not a ceiling any operator set.
            completion_check: None,
            baseline: None,
            budget: GoalBudget {
                usd: Some(Usd::ZERO),
                tokens: None,
            },
            hop_cap: 0,
            status: GoalStatus::Set,
            turns_on_goal: 0,
            last_result: None,
            first_done: None,
        }
    }
}

/// Mutate one row in place; a record naming a goal the table does not know is logged and
/// skipped, never a panic.
fn update(
    table: &mut RowTable<'_>,
    lsn: Lsn,
    id: GoalId,
    record: &Record,
    f: impl FnOnce(&mut GoalRow),
) -> Result<()> {
    let key = id.to_bytes();
    let Some(mut row) = get_row::<GoalRow>(table, &key)? else {
        tracing::warn!(target: "journal", lsn = %lsn, goal = %id, kind = ?record.kind(), "projection.unknown_goal");
        return Ok(());
    };
    f(&mut row);
    put_row(table, &key, &row)
}

/// Apply `entry` to the `goals` table.
///
/// `agents` is read, never written, and by exactly one arm: `LoopDecided{Done}` names an agent
/// and no goal, so the goal it achieved is the one its owner holds. That read is **order-
/// independent** — no arm of [`agents::apply`](super::agents::apply) writes `AgentRow.goal` for
/// a `LoopDecided` — which is why a shared reference is enough here and why this is not the
/// same situation as the budget tables, whose read of the `turns` table genuinely is ordered and
/// is therefore carried by a value rather than by this paragraph
/// (see [`EndedTurn`](super::turns::EndedTurn)).
#[expect(
    clippy::match_same_arms,
    reason = "the `Checkpoint` arm is deliberately identical to the wildcard: it exists so the change that teaches something to read a checkpoint has to visit this fold. `expect` rather than `allow` so that giving the arm a body of its own reports the stale attribute instead of leaving it behind"
)]
pub(crate) fn apply(
    table: &mut RowTable<'_>,
    first_done: &mut FirstDoneTable<'_>,
    agents: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
    let record = &entry.record;
    if let Some((agent, goal)) = record.goal_install() {
        let row = GoalRow {
            id: goal.id,
            agent,
            text: goal.text.clone(),
            predicate: goal.predicate.clone(),
            completion_check: goal.completion_check.clone(),
            baseline: goal.baseline.clone(),
            budget: goal.budget,
            hop_cap: goal.hop_cap,
            ..GoalRow::default()
        };
        return put_row(table, &goal.id.to_bytes(), &row);
    }
    match record {
        // The row stays, with its counters: `dudes why` still has to explain a goal after it was
        // abandoned, and a `GoalSet` reusing the id is refused before it is ever appended.
        Record::GoalCleared { goal, .. } => update(table, lsn, *goal, record, |g| {
            g.status = GoalStatus::Abandoned;
        }),
        // Owner moves; the `GoalId`, predicate, hop count and spend stay with the goal.
        // `budget_by_goal` gets this for free: it is keyed by the goal, so the key never moves.
        Record::GoalReassigned { goal, to, .. } => update(table, lsn, *goal, record, |g| {
            g.agent = *to;
        }),
        // Gate 6 counts managed turns only (an attached end counts toward
        // budget and *not* toward `hop_cap`). The wider counting question — re-issues, mixed mode —
        // is separate; this is the count the hop cap needs and nothing more.
        Record::TurnIssued {
            goal,
            mode: Mode::Managed,
            ..
        }
        | Record::NoteOnlyTurnIssued {
            goal,
            mode: Mode::Managed,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            goal,
            mode: Mode::Managed,
            ..
        }
        | Record::TurnIssuedWithBrowserNote {
            goal,
            mode: Mode::Managed,
            ..
        } => update(table, lsn, *goal, record, |g| {
            g.turns_on_goal = g.turns_on_goal.saturating_add(1);
            if g.status == GoalStatus::Set {
                g.status = GoalStatus::InProgress;
            }
        }),
        Record::PredicateResult {
            goal,
            provenance,
            pass,
            evidence,
            ..
        } => update(table, lsn, *goal, record, |g| {
            g.last_result = Some(PredicateOutcome {
                pass: *pass,
                provenance: *provenance,
                lsn,
                not_run: evidence.not_run(*pass),
            });
        }),
        // `Achieved` follows the loop's verdict, not the predicate's. Invariant 3 makes the two
        // agree — `Done` requires an `Observed` passing result — but they are different records,
        // and a goal whose predicate has passed while `decide()` has not yet acted on it is
        // `InProgress`, not achieved.
        Record::LoopDecided {
            agent,
            decision: LoopDecision::Done,
            ..
        } => {
            let Some(goal) = get_row::<AgentRow>(agents, &agent.to_bytes())?.and_then(|a| a.goal)
            else {
                tracing::warn!(target: "journal", lsn = %lsn, agent = %agent, "projection.done_without_a_goal");
                return note_fault(first_done, lsn);
            };
            let key = goal.to_bytes();
            let Some(mut row) = get_row::<GoalRow>(table, &key)? else {
                tracing::warn!(target: "journal", lsn = %lsn, goal = %goal, kind = ?record.kind(), "projection.unknown_goal");
                return note_fault(first_done, lsn);
            };
            row.status = GoalStatus::Achieved;
            let first = row.first_done.is_none();
            let observed = row
                .last_result
                .is_some_and(|result| result.pass && result.provenance.is_observed());
            if first && observed {
                row.first_done = Some(FirstDone { lsn, at: entry.at });
                first_done.insert(&fact_key(entry.at, goal), b"1".as_slice())?;
            }
            put_row(table, &key, &row)?;
            if first && !observed {
                tracing::warn!(target: "journal", lsn = %lsn, goal = %goal, "projection.done_without_an_observed_pass");
                return note_fault(first_done, lsn);
            }
            Ok(())
        }
        // Named, not left to the wildcard: see the note on the same arm in `agents.rs`.
        Record::Checkpoint { .. } => Ok(()),
        _ => Ok(()),
    }
}

fn note_fault(first_done: &mut FirstDoneTable<'_>, lsn: Lsn) -> Result<()> {
    first_done.insert(&fault_key(lsn), b"1".as_slice())?;
    Ok(())
}
