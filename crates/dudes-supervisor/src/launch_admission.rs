use dudes_types::AgentId;

/// Why owned-process evidence cannot admit a provider launch or shared-folder Claude inherit hire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SharedFolderRefusal {
    /// This agent still has an earlier physical target or launch uncertainty without durable absence.
    PriorAttemptUnresolved {
        /// The agent whose earlier attempt remains unresolved.
        agent: AgentId,
    },
    /// A full owned key currently identifies a live peer in the same canonical folder.
    LivePeer {
        /// Owning peer.
        agent: AgentId,
        /// Recorded display name.
        name: String,
    },
    /// The candidate folder could not be resolved as a directory.
    CandidateFolderUnresolved,
    /// A possible owned peer's execution folder could not be resolved.
    PeerFolderUnresolved {
        /// Owning peer.
        agent: AgentId,
    },
    /// A possibly relevant child has no complete key or current liveness answer.
    PeerLivenessUnknown {
        /// Owning peer.
        agent: AgentId,
    },
    /// A runtime receipt could not be joined to its issued owner.
    ChildOwnerUnresolved,
    /// Current child ownership exceeded the bounded admission census.
    ChildLimit,
}

impl std::fmt::Display for SharedFolderRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PriorAttemptUnresolved { agent } => write!(f,
                "An earlier owned attempt of agent {agent} has unresolved native liveness; wait for confirmed absence or use another agent in a worktree"),
            Self::LivePeer { agent, name } => write!(f,
                "Claude inherit is refused while {name} ({agent}) is live in the same folder; use a worktree"),
            Self::CandidateFolderUnresolved => f.write_str(
                "Claude inherit is refused because its working folder cannot be resolved; use a worktree once the folder is available"),
            Self::PeerFolderUnresolved { agent } => write!(f,
                "Claude inherit is refused because the working folder of agent {agent} cannot be resolved; use a worktree once its ownership is known"),
            Self::PeerLivenessUnknown { agent } => write!(f,
                "Claude inherit is refused because the owned child of agent {agent} has unresolved liveness; use a worktree or wait for confirmed absence"),
            Self::ChildOwnerUnresolved => f.write_str(
                "Claude inherit is refused because an owned child's agent cannot be resolved; use a worktree once ownership is known"),
            Self::ChildLimit => f.write_str(
                "Claude inherit is refused because the owned-child admission limit was reached; use a worktree or retry after confirmed child absence"),
        }
    }
}

impl std::error::Error for SharedFolderRefusal {}
