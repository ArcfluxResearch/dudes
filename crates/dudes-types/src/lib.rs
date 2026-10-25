//! Vocabulary of the Dudes supervisor: identities, enumerations, provenance and money.
//!
//! This crate has no behaviour worth the name — no I/O, no clock, no allocation beyond what a
//! `String` field needs. Every type here crosses a process boundary or lands in the journal, so
//! every type is `Serialize + Deserialize + JsonSchema`, is `Copy` where it can be and `Clone + Eq +
//! Hash` otherwise, and serialises with `snake_case` names. Adding a variant is a minor version;
//! removing or renaming one is a major version.
//!
//! Vocabulary is fixed: `Observed`/`Inferred`; `Managed`/`Attached`; `Tier 1` (hook-governed),
//! `Tier 2` (program-governed), `Tier 3` (observed-only); `Continue`/`Wait`/`Done`/`Exhausted`/
//! `Hold`; `Record`, `lsn`; `AgentId`, `TurnId`, `GoalId`, `CardId`, `MailId`. Agent lifetime is
//! much longer than turn lifetime: an agent outlives every turn and every goal it is given.
//!
//! The crate is `no_std` + `alloc` without the default `std` feature; the wire format is the same
//! either way.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod appearance;
pub mod boot;
pub mod display_name;
pub mod escaped;
pub mod id;
pub mod liveness;
pub mod mcp;
pub mod model;
pub mod money;
pub mod provenance;
pub mod provider;
pub mod status;
pub mod tier;
pub mod time;
pub mod worktree;

pub use appearance::{
    AccentColour, CastCharacter, ParseAccentColourError, ParseCastCharacterError,
};
pub use boot::HostBoot;
pub use display_name::{
    DisplayName, DisplayRole, InvalidDisplayName, InvalidDisplayRole, MAX_DISPLAY_NAME_BYTES,
    MAX_DISPLAY_ROLE_BYTES, MAX_IDENTITY_ROLE_BYTES, identity_name, identity_role,
};
pub use escaped::Escaped;
pub use id::{
    AgentId, CardId, GoalId, Lsn, MailId, ParseIdError, Seq, SessionId, ThreadId, TurnId,
};
pub use liveness::Liveness;
pub use mcp::{
    MAX_MCP_SERVER_NAME_CHARS, MCP_SERVER_NAME_PATTERN, McpServerName, ParseMcpServerNameError,
};
pub use model::{MAX_MODEL_NAME_BYTES, MODEL_NAME_PATTERN, ModelName, ParseModelNameError};
pub use money::{NegativeUsd, Tokens, Usage, Usd};
pub use provenance::Provenance;
pub use provider::{IsolationMode, ModelSelection, ParseProviderError, Provider};
pub use status::{
    AgentStatus, BreakerRung, BreakerSignal, CardStatus, GoalStatus, MailState, Mode, TurnStatus,
};
pub use tier::Tier;
pub use time::{Timestamp, UtcDay};
pub use worktree::{
    GitCommit, ParseGitCommitError, ParseWorktreeIdError, WORKTREE_ID_PATTERN, WorktreeId,
};
