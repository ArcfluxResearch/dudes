//! Complete pure inputs and actual work choices for a closing saving turn.

use dudes_proto::GateStep;
use dudes_types::{AgentId, GoalId, Lsn};
use serde::Serialize;

use crate::{Decided, LoopInput};

/// Every value used to decide one saving launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClosingLoopInput {
    /// Immutable closure identity.
    pub announcement: Lsn,
    /// Structural participant identity.
    pub agent: AgentId,
    /// The actual goal whose admission applies.
    pub goal: GoalId,
    /// The complete ordinary gate input, without altered controls or ceilings.
    pub ordinary: LoopInput,
    /// Actual task and carried answers, selected before deciding or hashing.
    pub selected_work: ClosingWorkChoice,
}

/// Saving work fits whole or waits without changing pending-answer delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClosingWorkChoice {
    /// The exact task and answer list that a Continue can issue.
    Ready {
        /// Complete launch text.
        task: String,
        /// Exactly the answers that text carries.
        answers: Vec<Lsn>,
    },
    /// No saving launch is admitted from this work choice.
    Deferred {
        /// Recomputable packing limit.
        reason: ClosingWorkDeferral,
    },
}

/// Why saving text cannot currently fit without weakening answer delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosingWorkDeferral {
    /// The unchanged answer block leaves insufficient room for the complete saving instruction.
    AnswersLeaveNoRoom,
}

/// The unchanged policy verdict or a work deferral that invents no goal verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClosingDecided {
    /// Existing gate result, with its exact selected task when it continues.
    Policy(Decided),
    /// All earlier gates passed but saving work must wait whole.
    Deferred {
        /// Consulted gates in the same fixed order.
        gate_trace: Vec<GateStep>,
        /// Recomputable task-room limit.
        reason: ClosingWorkDeferral,
    },
}

/// The same bounded saving order used at launch and at an admitted hook boundary.
#[must_use]
pub fn closing_notice(announcement: Lsn) -> String {
    closing_notice_for_commands(announcement, dudes_proto::TaskCommandSet::Legacy)
}

/// Saving guidance for the commands selected from the recorded hire.
#[must_use]
pub fn closing_notice_for_commands(
    announcement: Lsn,
    commands: dudes_proto::TaskCommandSet,
) -> String {
    if commands == dudes_proto::TaskCommandSet::ScopedFileMail {
        return "Closing time. Save or park current work and record current state and concrete next steps. Start no new work. Then exit this turn normally. This multi-repo ticket cannot acknowledge closing; the supervisor retains the 360-second grace for an unfinished participant.".to_owned();
    }
    format!(
        "Closing time. Save or park current work and record current state and concrete next steps. \
         Start no new work. While this process is still alive, run \
         `dudes closing ack --announcement-lsn {}`. Then exit this turn normally.",
        announcement.0
    )
}
