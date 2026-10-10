//! `ticket.*` params and results. Every change carries `expected_seq`, and a stale one is
//! refused as `stale_card`. The caller is never a field: the daemon resolves who is asking.

use dudes_types::{AgentId, CardId, Seq};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::projection::CardView;
use crate::record::Hex32;
use crate::ticket::TicketRow;

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
}

/// What became of an assigned ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketAssignOutcome {
    /// The ticket is in the agent's queue.
    Queued {
        /// Its place there, counting from 1.
        position: u32,
    },
}

/// `ticket.assign` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// `ticket.read` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketReadResult {
    /// The card as the board shows it.
    pub card: CardView,
    /// What the board keeps of the ticket.
    pub ticket: TicketRow,
    /// Its place in its holder's queue, counting from 1, while it waits there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}
