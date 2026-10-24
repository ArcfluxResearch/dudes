//! What a turn's end adds beside its outcome: a content-free count of the steps it took and the
//! size of the context it ended on. Both are evidence for display. No gate, predicate or breaker
//! reads either.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How many steps of each kind a turn took, and how many files its edit and write steps named.
///
/// It holds counts only: no tool name, input, path or result. It is what the engine reported, and
/// an agent can change files through a shell step without any edit step, so it can undercount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct StepDigest {
    /// Shell commands.
    pub command: u32,
    /// File reads.
    pub read: u32,
    /// Partial file edits.
    pub edit: u32,
    /// Whole-file writes.
    pub write: u32,
    /// File or name searches.
    pub search: u32,
    /// Web fetches and searches.
    pub web: u32,
    /// Plan updates.
    pub plan: u32,
    /// Helper-agent handoffs.
    pub subagent: u32,
    /// Calls to connected tool servers.
    pub mcp: u32,
    /// Anything else.
    pub other: u32,
    /// Distinct files the edit and write steps named.
    pub files_edited: u32,
}

impl StepDigest {
    /// Steps of every kind.
    #[must_use]
    pub fn total(&self) -> u32 {
        [
            self.command,
            self.read,
            self.edit,
            self.write,
            self.search,
            self.web,
            self.plan,
            self.subagent,
            self.mcp,
            self.other,
        ]
        .into_iter()
        .fold(0, u32::saturating_add)
    }
}

/// The context size an engine reported for a turn: how much of the prompt window its last model
/// call filled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextReport {
    /// The model the engine reported the window for, as it named it.
    pub model: String,
    /// The model's context window, in tokens.
    pub window: u64,
    /// Tokens the last model call sent as its prompt: fresh, cached and newly cached.
    pub used: u64,
}

/// What a managed turn's reader and the daemon add to its `TurnEnded`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnReport {
    /// Steps taken, when the engine's steps were read.
    pub step_digest: Option<StepDigest>,
    /// Context size, when the engine reported one.
    pub context: Option<ContextReport>,
}
