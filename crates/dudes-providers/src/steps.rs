//! The steps of a managed turn as an engine's reader reports them: the tool calls it made and
//! how each ended. Every type here is in no vendor's vocabulary, and none of it has been checked
//! for secrets or cut to a size. The daemon does both before anything leaves it, and a result's
//! text goes no further than the daemon's own store.

use dudes_types::SessionId;

/// What a step did, in the Floor's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// Ran a shell command.
    Command,
    /// Read a file.
    Read,
    /// Changed part of a file.
    Edit,
    /// Wrote a whole file.
    Write,
    /// Searched files or names.
    Search,
    /// Fetched or searched the web.
    Web,
    /// Wrote or updated the plan.
    Plan,
    /// Handed work to a helper agent.
    Subagent,
    /// Called a tool from a connected server.
    Mcp,
    /// Anything else.
    Other,
}

impl StepKind {
    /// The kind's name on the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Search => "search",
            Self::Web => "web",
            Self::Plan => "plan",
            Self::Subagent => "subagent",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }
}

/// How far one plan item has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Finished.
    Completed,
}

impl PlanStatus {
    /// The status's name on the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One line of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    /// What the item says.
    pub text: String,
    /// How far it has come.
    pub status: PlanStatus,
}

/// Text before and after an edit. A written file has no old text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditText {
    /// The text that was replaced.
    pub old: Option<String>,
    /// The text that replaced it.
    pub new: String,
}

/// One tool call, as the engine sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The engine's id for the call.
    pub id: String,
    /// The call this one ran under, for a helper agent's work.
    pub parent: Option<String>,
    /// What it did.
    pub kind: StepKind,
    /// The tool's name.
    pub tool: String,
    /// The arguments that are text, in the order the engine sent them.
    pub args: Vec<(String, String)>,
    /// An edit's text, for an edit or a write.
    pub edit: Option<EditText>,
    /// The plan, for a plan step.
    pub plan: Vec<PlanItem>,
}

/// A change to the steps of a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepEvent {
    /// The engine started a call.
    Open {
        /// The session the line named.
        session: SessionId,
        /// The call.
        step: Step,
    },
    /// The engine reported how a call ended.
    Close {
        /// The session the line named.
        session: SessionId,
        /// The call's id.
        id: String,
        /// Whether the call failed.
        failed: bool,
        /// The command's exit status, when the engine said.
        exit: Option<i32>,
        /// The size of the result in bytes.
        result_bytes: u64,
        /// The start of the result's text, as the engine sent it and cut to a size the daemon
        /// can scrub whole. It goes to the daemon's result store and never to a browser.
        result: Option<String>,
    },
}

impl StepEvent {
    /// The session the event's line named.
    #[must_use]
    pub const fn session(&self) -> &SessionId {
        match self {
            Self::Open { session, .. } | Self::Close { session, .. } => session,
        }
    }
}
