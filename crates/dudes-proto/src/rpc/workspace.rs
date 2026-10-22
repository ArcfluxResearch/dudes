//! `workspace.*` params and results.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `workspace.trust` params: trust or stop trusting one enrolled project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrustParams {
    /// The enrolled project's id.
    pub workspace_id: String,
    /// Whether the project is trusted from now on.
    pub trusted: bool,
}

/// `workspace.trust` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrustResult {
    /// Whether the project is trusted now.
    pub trusted: bool,
    /// Whether this call changed anything. An untrust of a project that was not trusted changes
    /// nothing and appends nothing.
    pub changed: bool,
}
