//! Current repository observations. These values never enter replayed state.

use std::collections::BTreeMap;

use dudes_types::{AgentId, Lsn};

use crate::world::AgentRow;

/// Exact hires whose current project configuration permits another execution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryAdmission {
    observed: BTreeMap<AgentId, (Lsn, bool)>,
}

impl RepositoryAdmission {
    /// A complete runtime observation, bound to the immutable hire positions.
    #[must_use]
    pub fn new(observed: impl IntoIterator<Item = (AgentId, Lsn, bool)>) -> Self {
        Self {
            observed: observed
                .into_iter()
                .map(|(agent, hire, allowed)| (agent, (hire, allowed)))
                .collect(),
        }
    }

    pub(crate) fn refuse(&mut self, agent: AgentId, hire: Lsn) {
        self.observed.insert(agent, (hire, false));
    }

    pub(crate) fn refused(&self, agent: AgentId, row: &AgentRow) -> bool {
        self.observed.get(&agent) == Some(&(row.mcp.hire_lsn, false))
    }

    pub(crate) fn allows(&self, agent: AgentId, row: &AgentRow) -> bool {
        if let Some((hire, allowed)) = self.observed.get(&agent) {
            return *hire == row.mcp.hire_lsn && *allowed;
        }
        row.repositories.workspace().is_none() && row.repositories.singleton().is_some()
    }
}
