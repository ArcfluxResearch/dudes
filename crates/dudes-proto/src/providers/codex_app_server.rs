//! Codex 0.160 app-server stdio payloads used by one managed turn.
//!
//! Source: `OpenAI`'s `rust-v0.160.0` app-server-protocol v2 types and the CLI's generated
//! TypeScript protocol. Unknown notification fields are skipped without retaining their bodies.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

/// JSON-RPC routing fields; the payload is read separately into its typed shape.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Envelope {
    /// Request or response identifier.
    pub id: Option<Value>,
    /// Request or notification method.
    pub method: Option<String>,
    /// Present on an unsuccessful response.
    pub error: Option<Value>,
}

/// Typed JSON-RPC response body.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Response<T: Default> {
    /// Successful response body.
    pub result: Option<T>,
}

/// Typed JSON-RPC notification body.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Notification<T: Default> {
    /// Notification parameters.
    pub params: Option<T>,
}

/// The initialization result identifies the installed server version.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Initialized {
    /// Vendor version and platform identifier.
    pub user_agent: Option<String>,
    /// Absolute home selected by the server.
    pub codex_home: Option<PathBuf>,
}

/// Shared stable fields of thread/start and thread/resume responses.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadResponse {
    /// Returned thread identity; historical turns are ignored.
    pub thread: Option<Thread>,
    /// Effective working directory.
    pub cwd: Option<PathBuf>,
    /// Effective approval policy.
    pub approval_policy: Option<String>,
    /// Effective sandbox policy.
    pub sandbox: Option<Sandbox>,
}

/// The identity of a created or resumed thread.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Thread {
    /// Provider-assigned thread id.
    pub id: Option<String>,
    /// Only an idle thread may receive this process's new task.
    pub status: Option<ThreadStatus>,
}

/// Loaded thread lifecycle status, before any task is submitted.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThreadStatus {
    /// `idle`, `active`, `notLoaded`, or `systemError`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// Stable workspace sandbox fields returned by thread/start.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Sandbox {
    /// `workspaceWrite` for the supported managed profile.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Additional writable roots; cwd is implicit.
    pub writable_roots: Option<Vec<PathBuf>>,
    /// Whether sandboxed tools may use the network.
    pub network_access: Option<bool>,
    /// Whether TMPDIR is excluded from implicit writable roots.
    pub exclude_tmpdir_env_var: Option<bool>,
    /// Whether /tmp is excluded from implicit writable roots.
    pub exclude_slash_tmp: Option<bool>,
}

/// turn/start result or turn/completed notification.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TurnResponse {
    /// Thread identity on a notification.
    pub thread_id: Option<String>,
    /// Current turn.
    pub turn: Option<Turn>,
}

/// Turn lifecycle fields; history and tool bodies are not retained.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Turn {
    /// Provider-assigned turn id.
    pub id: Option<String>,
    /// `inProgress`, `completed`, `interrupted`, or `failed`.
    pub status: Option<String>,
    /// Error presence prevents successful completion.
    pub error: Option<Value>,
    /// Authoritative summary items on turn completion.
    pub items: Option<Vec<Item>>,
}

/// item/started or item/completed notification.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ItemNotification {
    /// Owning thread.
    pub thread_id: Option<String>,
    /// Owning turn.
    pub turn_id: Option<String>,
    /// Item's visible fields.
    pub item: Option<Item>,
}

/// An app-server item; only agent message bodies are used by the reader.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Item {
    /// `agentMessage`, `reasoning`, `commandExecution`, and other item kinds.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Identity shared by the start, delta and completion events.
    pub id: Option<String>,
    /// Authoritative completed assistant text.
    pub text: Option<String>,
    /// `mcpToolCall`: the server whose tool was called.
    pub server: Option<String>,
}

/// item/agentMessage/delta notification.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentDelta {
    /// Owning thread.
    pub thread_id: Option<String>,
    /// Owning turn.
    pub turn_id: Option<String>,
    /// Previously started agent message item.
    pub item_id: Option<String>,
    /// Incremental text, concatenated verbatim within the item.
    pub delta: Option<String>,
}

/// thread/tokenUsage/updated notification.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(clippy::struct_field_names)] // Vendor field names.
pub struct TokenUsage {
    /// Owning thread.
    pub thread_id: Option<String>,
    /// Turn that caused this observation, excluding historical resume replay.
    pub turn_id: Option<String>,
    /// Usage reported by the vendor.
    pub token_usage: Option<TokenUsageBody>,
}

/// Only cumulative totals are used; `last` is not a per-Dudes-turn charge.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TokenUsageBody {
    /// Total usage for the provider session.
    pub total: Option<TokenTotals>,
}

/// Cumulative token components, as reported by Codex.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(clippy::struct_field_names)] // Vendor field names.
pub struct TokenTotals {
    /// Input tokens, including cached input.
    pub input_tokens: Option<u64>,
    /// Input tokens read from cache.
    pub cached_input_tokens: Option<u64>,
    /// Input tokens written to the cache.
    pub cache_write_input_tokens: Option<u64>,
    /// Output tokens, including reasoning output.
    pub output_tokens: Option<u64>,
}

/// turn/plan/updated notification: the plan for the turn, restated in full.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlanUpdate {
    /// Owning thread.
    pub thread_id: Option<String>,
    /// Owning turn.
    pub turn_id: Option<String>,
    /// Plan entries, each a `step` text with a `status` of `pending`, `inProgress` or `completed`.
    #[serde(deserialize_with = "super::null_as_default")]
    pub plan: Vec<Value>,
}
