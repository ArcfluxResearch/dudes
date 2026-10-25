//! Public team roster and the agent's per-call credential presentation.

use dudes_types::{AgentId, AgentStatus, CardId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::hook::HookToken;

/// Agent presentation for `team.roster`. Business parameters remain [`super::Empty`].
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RosterAgentParams {
    /// The per-call credential, resolved before business parameters are decoded.
    pub token: HookToken,
}

/// The activity a colleague may discover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RosterStatus {
    /// No work is issuing.
    Idle,
    /// Work is in progress.
    Working,
    /// The agent's work is complete.
    Done,
}

impl From<AgentStatus> for RosterStatus {
    fn from(status: AgentStatus) -> Self {
        match status {
            AgentStatus::Idle | AgentStatus::Exhausted => Self::Idle,
            AgentStatus::Running | AgentStatus::Waiting | AgentStatus::Attached => Self::Working,
            AgentStatus::Done => Self::Done,
        }
    }
}

/// The facts a colleague may discover about an unretired agent, and whether the row is the caller's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RosterEntry {
    /// Mail recipient identity.
    pub id: AgentId,
    /// Display name.
    pub name: String,
    /// Role text.
    pub role: String,
    /// Department.
    pub department: String,
    /// Public activity status.
    pub status: RosterStatus,
    /// Earliest claimed unfinished card, or null when none is held.
    pub held_card: Option<CardId>,
    /// True only on the row of the agent that asked, set by the daemon from the caller's token.
    #[serde(default)]
    pub you: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_discloses_exactly_seven_fields_even_without_a_card() {
        let row = RosterEntry {
            id: AgentId::NIL,
            name: "A".into(),
            role: "builder".into(),
            department: "engineering".into(),
            status: RosterStatus::Idle,
            held_card: None,
            you: true,
        };
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::json!({
                "id": AgentId::NIL, "name": "A", "role": "builder", "department": "engineering",
                "status": "idle", "held_card": null, "you": true,
            })
        );
    }

    #[test]
    fn roster_status_hides_private_lifecycle_reasons() {
        for (source, expected) in [
            (AgentStatus::Idle, RosterStatus::Idle),
            (AgentStatus::Running, RosterStatus::Working),
            (AgentStatus::Waiting, RosterStatus::Working),
            (AgentStatus::Exhausted, RosterStatus::Idle),
            (AgentStatus::Done, RosterStatus::Done),
            (AgentStatus::Attached, RosterStatus::Working),
        ] {
            assert_eq!(RosterStatus::from(source), expected);
        }
    }
}
