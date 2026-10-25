//! What `decide()` reads. Every field is a value the supervisor has already journaled or loaded
//! from configuration, so a decision is reproducible from the journal alone (invariant 5).
//!
//! `LoopInput` never crosses a process boundary: the journal stores its digest in
//! `LoopDecided.input_digest` and replay rebuilds it from the projection. It is therefore not
//! contract surface and carries no JSON Schema; `Serialize` exists so the supervisor can hash its
//! canonical bytes.

use dudes_proto::{
    AgentBudget, CheckBaselineKind, GateStep, GoalBudget, HoldReason, LoopDecision, Spend,
};
use dudes_types::{AgentId, BreakerRung, CardId, CardStatus, Lsn, Provenance, Usd};
use serde::Serialize;

use crate::budget::{BudgetState, Ceilings};

/// Everything one decision for one agent depends on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoopInput {
    /// Standing control flags (gate 1).
    pub control: Control,
    /// Provenance of the last `TurnEnded`; `None` before any turn (gate 2).
    pub last_turn_end: Option<Provenance>,
    /// A `HumanQuestion` without a `HumanAnswer` (gate 3).
    pub open_question: Option<String>,
    /// The goal the agent pursues (gates 4, 5, 6, 7).
    pub goal: GoalInput,
    /// The agent's own ceiling, spend and breaker rung (gate 5).
    pub agent: AgentInput,
    /// The floor-wide ceiling and spend (gate 5).
    pub floor: FloorInput,
    /// Configured price of each journaled unobserved end (gate 5).
    pub per_turn_floor: Spend,
    /// What a turn would work on (gate 7).
    pub work: WorkInput,
    /// Answers to this agent's earlier questions that no spawned turn has carried yet, oldest
    /// first. They take the head of gate 7's task and move no gate.
    ///
    /// Omitted from the canonical bytes while empty, so the digest of every input without an
    /// answer is the one it had before answers existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<AnswerInput>,
}

/// One answer, as the supervisor's fold holds it until a spawned turn consumes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AnswerInput {
    /// Where its `HumanAnswer` sits: the position a carrying `TurnIssued.answers` names.
    pub lsn: Lsn,
    /// The question it answered, as it was asked.
    pub question: String,
    /// The answer, verbatim.
    pub text: String,
}

/// Control flags from `Halt` / `Pause` / `Resume` / `Attached` / `Detached` records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Control {
    /// `loop.halt` is standing.
    pub halted: bool,
    /// `loop.pause` is standing.
    pub paused: bool,
    /// A human holds the keyboard.
    pub attached: bool,
    /// A journaled automatic hold, after existing control priorities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_hold: Option<HoldReason>,
}

/// The goal as the projection holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoalInput {
    /// Its text, the task when no card is claimable.
    pub text: String,
    /// The hop cap (gate 6).
    pub hop_cap: u32,
    /// Managed `TurnIssued` records for this goal so far; counted by the projection.
    pub turns_on_goal: u32,
    /// Its ceiling.
    pub budget: GoalBudget,
    /// Spend against it so far.
    pub spent: Spend,
    /// Unobserved ends against this goal.
    pub unknown_ends: u32,
    /// The latest predicate result, when one exists.
    pub predicate: Option<PredicateInput>,
    /// What goal set saw of the check before the first turn. Only gate 4's note reads it, and it
    /// is left out of the input when absent, so the input digest keeps its bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<CheckBaselineKind>,
}

/// The latest `PredicateResult` for the goal, reduced to what gate 4 reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PredicateInput {
    /// Whether it passed.
    pub pass: bool,
    /// Whether the daemon observed it.
    pub provenance: Provenance,
    /// The check's program did not start, so the result judged nothing. Only gate 4's note reads
    /// it, and it is left out of the input when false, so the input digest keeps its bytes.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_run: bool,
}

/// The agent's ceiling, spend today and breaker rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AgentInput {
    /// Its daily ceiling.
    pub budget: AgentBudget,
    /// Spend today, already rolled over by the projection.
    pub spent_today: Spend,
    /// Unobserved ends today.
    pub unknown_ends_today: u32,
    /// Current breaker rung.
    pub rung: BreakerRung,
}

/// The floor-wide daily ceiling (`[budget] floor_usd_per_day`) and spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FloorInput {
    /// Dollars per day across every agent, when the operator configured a floor ceiling.
    pub usd_per_day: Option<Usd>,
    /// Spend today across every agent.
    pub spent_today: Spend,
    /// Unobserved ends today across the floor.
    pub unknown_ends_today: u32,
}

/// What the next turn would work on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WorkInput {
    /// A claimable card for this role with its dependencies satisfied, if any.
    pub claimable_card: Option<CardInput>,
    /// Live coordination context for the earliest held card.
    pub held_card_context: Option<HeldCardContext>,
    /// Last complete sample of published inbox markers, bounded to 32.
    pub unread_mail: u32,
    /// The memory digest to prepend to the goal text; empty when there is none.
    pub memory_digest: String,
    /// Who the agent is, as its stored name and role make its identity line. Set from the agent's
    /// own row, never from live status, so only a rename or a role change moves it. Absent only
    /// where no agent row exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<dudes_proto::IdentityLine>,
}

/// Live coordination context for one held card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HeldCardContext {
    /// The held card.
    pub card: CardId,
    /// Unfinished cards that depend on this card, in canonical id order.
    pub waiting_on_me: Vec<ReverseDependent>,
    /// Other live agents in the same department, in canonical id order.
    pub peers: Vec<DepartmentPeer>,
}

/// An unfinished card that depends on the held card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReverseDependent {
    /// The dependent card.
    pub card: CardId,
    /// Its title.
    pub title: String,
    /// Its current status.
    pub status: CardStatus,
    /// Its current holder.
    pub held_by: Option<AgentId>,
}

/// Another live agent in the held card's department.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DepartmentPeer {
    /// The peer.
    pub agent: AgentId,
    /// Its display name.
    pub name: String,
    /// Its earliest unfinished held card.
    pub held_card: Option<CardId>,
}

/// A board card, reduced to what the task text needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CardInput {
    /// The card.
    pub id: CardId,
    /// Its title.
    pub title: String,
}

impl LoopInput {
    /// The spend gate 5 admits against.
    #[must_use]
    pub const fn budget_state(&self) -> BudgetState {
        BudgetState {
            goal: self.goal.spent,
            goal_unknown_ends: self.goal.unknown_ends,
            agent_today: self.agent.spent_today,
            agent_unknown_ends_today: self.agent.unknown_ends_today,
            floor_today: self.floor.spent_today,
            floor_unknown_ends_today: self.floor.unknown_ends_today,
        }
    }

    /// The ceilings gate 5 admits against.
    #[must_use]
    pub const fn ceilings(&self) -> Ceilings {
        Ceilings {
            goal: self.goal.budget,
            agent: self.agent.budget,
            floor_usd_per_day: self.floor.usd_per_day,
        }
    }
}

/// What `decide()` returns: the verdict and the gates consulted to reach it, in order. Both go
/// into `LoopDecided`, which `dudes why` reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Decided {
    /// The verdict.
    pub decision: LoopDecision,
    /// Gates consulted, in order; exactly the last one fired.
    pub gate_trace: Vec<GateStep>,
    /// The answers a `Continue` task carries, oldest first — what the turn it issues lists in
    /// `TurnIssued.answers`. Empty for every other verdict.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<Lsn>,
}
