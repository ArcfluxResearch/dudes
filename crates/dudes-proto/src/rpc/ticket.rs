//! `ticket.*` params and results. Every change carries `expected_seq`, and a stale one is
//! refused as `stale_card`. The caller is never a field: the daemon resolves who is asking.

use std::path::PathBuf;

use dudes_types::{AgentId, CardId, Seq, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::instruction::InstructionReviewWatermark;
use crate::projection::CardView;
use crate::record::Hex32;
use crate::ticket::{TicketReason, TicketRow, TicketWaiting};

mod context;
pub use context::*;

/// `ticket.create` params. Only the title is required.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketCreateParams {
    /// Title: 1 to 256 bytes after trimming.
    pub title: String,
    /// The note: at most 8,192 bytes after trimming. Absent means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The enrolled project. Absent means the only enrolled project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The id of one of the project's enrolled checks. Absent means the project's own check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_when: Option<String>,
    /// The revision of that check the caller looked at; a changed check is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub done_when_revision: Option<Hex32>,
}

/// `ticket.create` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketCreateResult {
    /// The new ticket.
    pub card: CardId,
}

/// `ticket.edit` params. At least one of the three values changes. Valid while the ticket is in
/// To do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketEditParams {
    /// The ticket.
    pub card: CardId,
    /// The new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The new note; an empty note clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the project's check to finish the ticket by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_when: Option<String>,
    /// The revision of that check the caller looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub done_when_revision: Option<Hex32>,
    /// The ticket's `seq` as last seen.
    pub expected_seq: Seq,
}

/// `ticket.assign` params: put the ticket in an agent's queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketAssignParams {
    /// The ticket.
    pub card: CardId,
    /// The agent, hired into the ticket's project.
    pub agent: AgentId,
    /// What to tell the agent when sending work back: at most 2,048 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The ticket's `seq` as last seen.
    pub expected_seq: Seq,
    /// Exact instruction set reviewed by Ask for changes. Absent for ordinary assignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_watermark: Option<InstructionReviewWatermark>,
}

/// What became of an assigned ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketAssignOutcome {
    /// The agent is working on it.
    Started,
    /// The ticket is in the agent's queue and has not started.
    Queued {
        /// Its place there, counting from 1.
        position: u32,
        /// Why it has not started.
        waiting: TicketWaiting,
    },
    /// The ticket could not start for a reason only the operator can fix, and left the queue.
    NeedsYou {
        /// The reason, as the board shows it.
        reason: TicketReason,
        /// The sentence that names the next step.
        note: String,
    },
}

/// `ticket.assign` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketAssignResult {
    /// What became of it.
    pub outcome: TicketAssignOutcome,
}

/// `ticket.remove` params. A ticket that is being worked on must be stopped first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketRemoveParams {
    /// The ticket.
    pub card: CardId,
    /// The ticket's `seq` as last seen.
    pub expected_seq: Seq,
}

/// `ticket.read` params.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketReadParams {
    /// The ticket.
    pub card: CardId,
}

/// `ticket.checkout` params. The observed card sequence binds the recorded claimant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketCheckoutParams {
    /// The ticket.
    pub card: CardId,
    /// The card sequence the operator observed.
    pub expected_seq: Seq,
}

/// The recorded layout needed to revalidate one leased checkout locally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketCheckoutLease {
    /// The daemon-created worktree identity.
    pub worktree: WorktreeId,
    /// The recorded origin repository.
    pub origin: PathBuf,
}

/// `ticket.checkout` result. Paths are returned only to the local operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketCheckoutResult {
    /// The absolute recorded working directory, without walking to a parent repository.
    pub directory: PathBuf,
    /// A lease whose `.git` and `commondir` layout the CLI must revalidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<TicketCheckoutLease>,
}

/// `ticket.read` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketReadResult {
    /// Derived recovery and check display facts at this read's cut.
    #[serde(default)]
    pub context: TicketReadContext,
    /// The card as the board shows it.
    pub card: CardView,
    /// What the board keeps of the ticket.
    pub ticket: TicketRow,
    /// Its place in its holder's queue, counting from 1, while it waits there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// Why a ticket that was given to an agent has not started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<TicketWaiting>,
}

/// `ticket.stop` params: take a ticket that is Working or in Needs you back to To do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketStopParams {
    /// The ticket.
    pub card: CardId,
    /// The ticket's `seq` as last seen.
    pub expected_seq: Seq,
}

/// `ticket.accept` params. Instruction review acknowledges the named set and accepts completed
/// work only when existing observed goal-completion evidence permits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketAcceptParams {
    /// The ticket.
    pub card: CardId,
    /// The ticket's `seq` as last seen.
    pub expected_seq: Seq,
    /// Exact instruction set reviewed. Absent for ordinary completed-ticket acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_watermark: Option<InstructionReviewWatermark>,
}

/// `ticket.accept` result. Ordinary completed acceptance remains exactly `{}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketAcceptResult {
    /// Present when the action acknowledged instructions and the work continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TicketAcceptOutcome>,
}

/// The explicit continuing-work outcome of instruction acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketAcceptOutcome {
    /// The named instruction review was acknowledged; the ticket is still work in progress.
    InstructionsAcknowledged,
}
