//! What `dudes_policy::decide()` returns and the trace of gates that produced it. Journaled as
//! `LoopDecided`, which is what `dudes why` reads.

use dudes_types::{TurnId, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A durable automatic cause for holding an agent until Resume or a new goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomaticHold {
    /// Two consecutive observed fast failures without positive usage.
    RepeatedFastFailure {
        /// The first failing turn.
        first: TurnId,
        /// The second failing turn.
        latest: TurnId,
    },
    /// The adapter refused a task before a process launch.
    LaunchRefused {
        /// Safe adapter error, without launch inputs.
        reason: String,
    },
    /// The host confirmed that the issued turn could not launch.
    LaunchFailed {
        /// Bounded host error, without launch inputs.
        reason: String,
    },
    /// The recorded worktree of a restarted agent is gone or is no longer a worktree of its
    /// repository. The daemon never falls back to the repository itself.
    WorktreeMissing {
        /// The lease whose tree is missing. Its path is never a hold input.
        worktree: WorktreeId,
    },
    /// A cause a later version wrote and this one does not know. It holds the agent, because
    /// admitting a turn under a cause nothing here can read would be the unsafe reading.
    #[serde(other)]
    Unknown,
}

impl AutomaticHold {
    /// The unit reason read by the pure control gate.
    #[must_use]
    pub const fn hold_reason(&self) -> HoldReason {
        match self {
            Self::RepeatedFastFailure { .. } => HoldReason::RepeatedFastFailure,
            Self::LaunchRefused { .. } => HoldReason::LaunchRefused,
            Self::LaunchFailed { .. } => HoldReason::LaunchFailed,
            Self::WorktreeMissing { .. } => HoldReason::WorktreeMissing,
            Self::Unknown => HoldReason::UnknownCause,
        }
    }
}

impl std::fmt::Display for AutomaticHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepeatedFastFailure { first, latest } => write!(
                f,
                "repeated fast failures ({first}, {latest}); fix the cause and resume"
            ),
            Self::LaunchRefused { reason } => {
                write!(f, "launch refused: {reason}; fix the cause and resume")
            }
            Self::LaunchFailed { reason } => {
                write!(f, "launch failed: {reason}; fix the cause and resume")
            }
            Self::WorktreeMissing { worktree } => write!(
                f,
                "worktree {worktree} is missing; restore it or retire the agent, then resume"
            ),
            Self::Unknown => f.write_str("held for a cause this version does not know; resume"),
        }
    }
}

/// The loop's verdict for one agent at one moment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopDecision {
    /// Issue a managed turn with this task.
    Continue {
        /// The task text: a claimable card, or the goal text with unread-mail count and
        /// memory digest prepended.
        task: String,
    },
    /// A human must answer first.
    Wait {
        /// The open question.
        question: String,
    },
    /// The goal's predicate passed with `Observed` provenance.
    Done,
    /// A ceiling refused admission.
    Exhausted {
        /// Which one.
        reason: ExhaustedReason,
    },
    /// Issue nothing; a control flag or a provenance doubt stands in the way.
    Hold {
        /// Which one.
        reason: HoldReason,
    },
}

/// Finite input admission, whose verdict cannot complete or reopen a goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoteAdmission {
    /// Issue one charged Managed task under the carrying record's contract.
    Issue {
        /// The immutable complete task.
        task: String,
    },
    /// Keep the note pending while a human question is open.
    Wait {
        /// The open question.
        question: String,
    },
    /// Refuse admission under the existing control or provenance gate.
    Hold {
        /// The fixed refusal reason.
        reason: HoldReason,
    },
    /// Refuse admission under the existing budget or hop gate.
    Exhausted {
        /// The fixed refusal reason.
        reason: ExhaustedReason,
    },
}

/// Why a goal is `Exhausted`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustedReason {
    /// The goal's budget.
    GoalBudget,
    /// The agent's daily budget.
    AgentBudget,
    /// The floor's daily budget.
    FloorBudget,
    /// Breaker rung at `Constrain` or above.
    Breaker,
    /// `turns_on_goal` reached `hop_cap`.
    Hops,
    /// A visible billing credential requires an applicable spending ceiling.
    CeilingRequired,
    /// Twenty completed-goal mail-wake issues already belong to the issue's UTC day.
    MailWakeLimit,
}

/// Why the loop issued nothing (gates 1 and 2).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// `loop.halt` is standing.
    Halted,
    /// `loop.pause` is standing.
    Paused,
    /// A human holds the keyboard.
    Attached,
    /// The last turn end is `Inferred`; a breaker signal is raised and `Done` is unreachable
    /// until its emitter's Observed end — a second `TurnEnded` for the same `TurnId` with
    /// `provenance: Observed` — supersedes it. No v1 emitter writes an
    /// `Inferred` end, so no v1 run reaches this reason; the gate stays because `decide()` must
    /// not know which emitters exist (invariant 5).
    InferredTurnEnd,
    /// Two consecutive fast failed turns require operator recovery.
    RepeatedFastFailure,
    /// The adapter refused a launch.
    LaunchRefused,
    /// The host could not launch an issued turn.
    LaunchFailed,
    /// The agent's recorded worktree is missing.
    WorktreeMissing,
    /// A hold cause this version does not know.
    UnknownCause,
    /// Associated instruction comparison or exact Operator review remains outstanding.
    /// Ordinary Resume cannot acknowledge this journal-derived obligation.
    SharedInstructionsReview,
}

/// The seven gates of `decide()`, in the order they are consulted (invariant 5).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Halted, paused or attached → `Hold`.
    Control,
    /// Last turn end `Inferred` → `Hold` plus a breaker signal.
    Provenance,
    /// Open `HumanQuestion` → `Wait`.
    Human,
    /// Latest predicate result `Observed` and passing → `Done`.
    Done,
    /// Goal, agent or floor ceiling, or breaker rung ≥ `Constrain` → `Exhausted`.
    Budget,
    /// `turns_on_goal ≥ hop_cap` → `Exhausted`.
    Hops,
    /// A claimable card or the goal text → `Continue`.
    Work,
}

impl Gate {
    /// The gate order. `decide()` walks it top to bottom; the first gate that fires decides.
    pub const ORDER: [Self; 7] = [
        Self::Control,
        Self::Provenance,
        Self::Human,
        Self::Done,
        Self::Budget,
        Self::Hops,
        Self::Work,
    ];
}

/// One gate's entry in a decision's trace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GateStep {
    /// Which gate.
    pub gate: Gate,
    /// Whether this gate produced the decision (only the last step fires).
    pub fired: bool,
    /// A short human-readable note for `dudes why` (`"spent $9.80 of $10.00"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant 5: gate order is control → provenance → human → done → budget → hops → work.
    #[test]
    fn gate_order_is_fixed() {
        assert_eq!(
            Gate::ORDER,
            [
                Gate::Control,
                Gate::Provenance,
                Gate::Human,
                Gate::Done,
                Gate::Budget,
                Gate::Hops,
                Gate::Work
            ]
        );
        assert!(Gate::ORDER.windows(2).all(|w| w[0] < w[1]));
    }

    /// Vocabulary: the five decisions are `continue` / `wait` / `done` / `exhausted` / `hold`,
    /// every one round-trips, and the sample count matches the schema.
    #[test]
    fn every_decision_round_trips_with_the_fixed_vocabulary() {
        let samples = [
            LoopDecision::Continue { task: "t".into() },
            LoopDecision::Wait {
                question: "q".into(),
            },
            LoopDecision::Done,
            LoopDecision::Exhausted {
                reason: ExhaustedReason::Hops,
            },
            LoopDecision::Hold {
                reason: HoldReason::Paused,
            },
        ];
        let schema = schemars::schema_for!(LoopDecision);
        let variants = schema.as_value()["oneOf"].as_array().unwrap().len();
        assert_eq!(variants, samples.len(), "a LoopDecision has no sample");
        let mut names = Vec::new();
        for decision in samples {
            let json = serde_json::to_value(&decision).unwrap();
            names.push(json["kind"].as_str().unwrap().to_owned());
            assert_eq!(
                serde_json::from_value::<LoopDecision>(json).unwrap(),
                decision
            );
        }
        assert_eq!(names, ["continue", "wait", "done", "exhausted", "hold"]);
        assert_eq!(
            serde_json::to_value(LoopDecision::Exhausted {
                reason: ExhaustedReason::AgentBudget
            })
            .unwrap(),
            serde_json::json!({"kind": "exhausted", "reason": "agent_budget"})
        );
    }
}
