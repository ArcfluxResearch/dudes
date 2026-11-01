use dudes_types::{AgentId, IsolationMode, Lsn, McpServerName, TurnId};

use crate::host::AdoptionKey;
use dudes_proto::Record;

use super::World;

/// Recorded origin and the desired server set for later managed launches.
#[derive(Clone, Debug)]
pub struct McpPolicy {
    /// The position the agent's current place was recorded at: its hire, or its latest move.
    pub hire_lsn: Lsn,
    /// Immutable names for historical launches without a grant.
    pub hire_servers: Vec<McpServerName>,
    /// Original explicit isolation choice.
    pub isolation: Option<IsolationMode>,
    /// The recorded hire came from browser onboarding.
    pub browser_onboarded: bool,
    /// Canonical desired names.
    pub desired: Vec<McpServerName>,
    /// Hire position, or the latest replacement position.
    pub revision: Lsn,
    /// First replacement, including a clear; absent for an unchanged hire.
    pub first_edit: Option<Lsn>,
}

impl McpPolicy {
    pub(super) fn hired(
        lsn: Lsn,
        names: &[McpServerName],
        isolation: Option<IsolationMode>,
        browser_onboarded: bool,
    ) -> Self {
        Self {
            hire_lsn: lsn,
            hire_servers: names.to_vec(),
            isolation,
            browser_onboarded,
            desired: names.to_vec(),
            revision: lsn,
            first_edit: None,
        }
    }

    pub(super) fn replace(&mut self, lsn: Lsn, names: &[McpServerName]) {
        self.desired = names.to_vec();
        self.revision = lsn;
        self.first_edit.get_or_insert(lsn);
    }
}

/// Immutable permission for one managed launch, keyed by its journal position.
#[derive(Clone, Debug)]
pub struct McpGrant {
    /// Owning agent.
    pub agent: AgentId,
    /// Issued turn.
    pub turn: TurnId,
    /// Desired revision selected at admission.
    pub policy_lsn: Lsn,
    /// Names selected before the grant was persisted.
    pub mcp_servers: Vec<McpServerName>,
    /// A matching spawn receipt, rather than evidence derived from the grant itself.
    pub spawned: Option<(Lsn, AdoptionKey)>,
}

/// A required MCP record contradicts the preceding journal history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpHistoryError {
    /// The edit or grant has no recorded live agent.
    UnknownAgent,
    /// Changed browser-onboarded policy is outside local edit authority.
    BrowserOrigin,
    /// Names are not sorted and unique.
    InvalidSet,
    /// A replacement did not change the desired set.
    UnchangedSet,
    /// A grant does not name this agent's recorded turn.
    UnknownTurn,
    /// A grant does not name the current edited policy and names.
    InvalidPolicy,
    /// A spawn names no preceding grant for its agent and turn.
    WrongGrant,
    /// A spawn after the first edit has no grant.
    MissingGrant,
    /// One grant was paired with a different physical child.
    ReusedGrant,
}

impl std::fmt::Display for McpHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnknownAgent => "MCP record has no recorded live agent",
            Self::BrowserOrigin => "MCP replacement is outside the original browser hire's scope",
            Self::InvalidSet => "MCP record names are not sorted and unique",
            Self::UnchangedSet => "MCP replacement does not change the desired set",
            Self::UnknownTurn => "MCP grant does not name the agent's recorded turn",
            Self::InvalidPolicy => "MCP grant does not match the current edited policy",
            Self::WrongGrant => "spawn does not name a preceding MCP grant for its agent and turn",
            Self::MissingGrant => "spawn after an MCP edit has no launch grant",
            Self::ReusedGrant => "MCP grant is paired with a different physical child",
        })
    }
}

impl std::error::Error for McpHistoryError {}

impl World {
    /// The immutable permission at its own journal identity.
    #[must_use]
    pub fn mcp_grant(&self, lsn: Lsn) -> Option<&McpGrant> {
        self.mcp_grants.get(&lsn)
    }

    /// Validate required MCP relationships before replay folds a record or starts recovery.
    ///
    /// # Errors
    /// Returns the first contradiction of the preceding recorded authority.
    pub fn validate_mcp_record(&self, lsn: Lsn, record: &Record) -> Result<(), McpHistoryError> {
        match record {
            Record::AgentMcpServersChanged { agent, mcp_servers } => {
                let policy = self
                    .agents
                    .get(agent)
                    .filter(|row| !row.retired)
                    .ok_or(McpHistoryError::UnknownAgent)?;
                if policy.mcp.browser_onboarded {
                    return Err(McpHistoryError::BrowserOrigin);
                }
                canonical(mcp_servers)?;
                if policy.mcp.desired == *mcp_servers {
                    return Err(McpHistoryError::UnchangedSet);
                }
            }
            Record::McpLaunchGranted {
                agent,
                turn,
                policy_lsn,
                mcp_servers,
            } => {
                let row = self
                    .agents
                    .get(agent)
                    .filter(|row| !row.retired)
                    .ok_or(McpHistoryError::UnknownAgent)?;
                canonical(mcp_servers)?;
                if row.mcp.first_edit.is_none()
                    || row.mcp.revision != *policy_lsn
                    || row.mcp.desired != *mcp_servers
                    || *policy_lsn >= lsn
                {
                    return Err(McpHistoryError::InvalidPolicy);
                }
                if self
                    .turns
                    .get(turn)
                    .is_none_or(|row| row.agent != *agent || row.has_ended())
                {
                    return Err(McpHistoryError::UnknownTurn);
                }
            }
            Record::TurnSpawned {
                agent,
                turn,
                pid,
                start_time,
                boot,
                mcp_grant,
                ..
            } => {
                if let Some(grant_lsn) = mcp_grant {
                    if self
                        .turns
                        .get(turn)
                        .is_none_or(|row| row.agent != *agent || row.has_ended())
                    {
                        return Err(McpHistoryError::WrongGrant);
                    }
                    let grant = self
                        .mcp_grants
                        .get(grant_lsn)
                        .filter(|grant| {
                            *grant_lsn < lsn && grant.agent == *agent && grant.turn == *turn
                        })
                        .ok_or(McpHistoryError::WrongGrant)?;
                    if grant.spawned.as_ref().is_some_and(|(_, key)| {
                        key.pid != *pid || key.start_time != *start_time || key.boot != *boot
                    }) {
                        return Err(McpHistoryError::ReusedGrant);
                    }
                } else if self
                    .agents
                    .get(agent)
                    .is_some_and(|row| row.mcp.first_edit.is_some())
                {
                    return Err(McpHistoryError::MissingGrant);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn canonical(names: &[McpServerName]) -> Result<(), McpHistoryError> {
    if names
        .windows(2)
        .any(|pair| matches!(pair, [first, second] if first >= second))
    {
        return Err(McpHistoryError::InvalidSet);
    }
    Ok(())
}
