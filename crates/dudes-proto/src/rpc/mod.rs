//! The RPC surface: JSON-RPC 2.0, newline-delimited, one method table for every client.
//!
//! [`Request`], [`Response`] and [`Notification`] are the envelopes. Every method's params and
//! result are typed in the submodules and listed once in [`Method`]; the generated
//! `schemas/rpc.schema.json` is the source of truth for the floor's TypeScript client.
//!
//! Identity is structural (invariant 8), and the peer credential is only half of it: it admits
//! the connection (same uid) and **cannot tell an operator from an agent** — same uid is not a
//! boundary. An **operator** is the [`cred::OperatorCredential`] it presents on the envelope. An
//! **agent** is the per-agent token it presents, per call, on every agent-callable method —
//! `hook.event`, `memory.append`, `memory.read`, `board.claim`, `board.transition`, `team.roster` — and the
//! server derives the agent from that token, never from a field naming one. What
//! any presentation resolves to — including presenting nothing — is one table, the peer-classification table
//! that `dudes_rpc::auth::classify` implements, cited here and never restated. There is no handshake and no header frame: the
//! agent-side clients open one connection per call, so a connection-level binding would bind
//! state to nothing reusable. Where a params field names an agent (`board.claim`, `memory.read`)
//! it is ignored on a call that carries a token, and names the agent acted on from an operator
//! peer.
//!
//! # Params fail closed; results and notifications do not
//!
//! **Every params type in [`Methods`] is `#[serde(deny_unknown_fields)]`.** Params are what an
//! operator types at, by hand and once: a key we silently ignore is a setting they believe they
//! made, and a misspelling then looks exactly like a value that was read and did nothing. That is
//! the same reason `[providers.claude]` denies unknown keys, and it is the *opposite* of the rule
//! for an inbound vendor payload, which is `#[serde(default)]` and never strict because a vendor
//! adds fields on its own schedule ([`crate::providers`] states that one).
//!
//! The rule stops at three edges, each for the growth reason rather than a taste:
//!
//! - **Results and notification params are not strict.** They travel daemon → client, so a reader
//!   that refused an unknown key would refuse the daemon's own growth — and adding a field is a
//!   *minor* change by this crate's own rule. [`Empty`] is the one type on both sides, and it is
//!   strict: it cannot grow a field in either role, because it is shared by every method that
//!   answers `ok`, so denying costs it nothing it could otherwise have.
//! - **A nested type is not strict.** The attribute governs one object's own keys and does not
//!   descend; several nested types ([`crate::budget::AgentBudget`], [`crate::goal::Predicate`])
//!   are also journal payloads, where refusing an unknown field would refuse a *newer* record
//!   instead of degrading.
//! - **A vendor payload is never strict**, wherever it is reached. `hook.event` carries it as
//!   `raw: Value` for exactly that reason, so the params object can deny while its cargo cannot.
//!
//! `every_method_params_type_refuses_unknown_fields` is the scan that keeps the first sentence
//! true for a method added later, and `the_attribute_stops_at_the_params_object` pins the last
//! two edges. The first edge is a claim about what is *absent*, and the schema diff is where it
//! is read: no result-**only** definition gained `additionalProperties: false`. [`Empty`] did,
//! and it is the sole type in both columns, for the reason given above.

pub mod agent;
pub mod board;
pub mod closing;
pub mod cred;
pub mod floor;
pub mod goal;
pub mod hook;
pub mod journal;
pub mod mail;
pub mod memory;
pub mod pty;
mod secret;
pub mod system;
pub mod team;
pub mod ticket;
pub mod worktree;

use std::fmt;
use std::str::FromStr;

use dudes_types::{AgentId, Provider, Seq};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON-RPC version string every envelope carries.
pub const JSONRPC: &str = "2.0";

/// The largest newline-delimited RPC frame the contract admits, including its terminating
/// newline.
///
/// This is the transport admission ceiling, not the journal's on-disk format ceiling. The
/// journal keeps a wider readable format but preflights the exact [`journal::journal_appended`]
/// value against this limit before making a new record durable.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A request id: a number or a string, echoed on the response.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id.
    Number(i64),
    /// String id.
    String(String),
}

/// A call from a client to the daemon.
// No `Clone`, and the reason is not shape: `cred` carries a secret, `OperatorCredential` has no
// `Clone` for the reason recorded on that type, and an envelope holding one inherits it — a
// request that could be duplicated by inference is a secret whose copies nobody counted. A `//`
// comment rather than a doc comment because this is Rust's, and the doc comments in this file are
// exported into `schemas/rpc.schema.json` for clients that are not.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoed on the response.
    pub id: RequestId,
    /// A [`Method`] name.
    pub method: String,
    /// The method's params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Caller contract major version ([`crate::CONTRACT_VERSION`]). `goal.replace` refuses
    /// unsupported majors; legacy methods do not gate on this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<u16>,
    /// The operator credential this call presents, if it presents one.
    ///
    /// One optional field on the envelope rather than a param on each operator method: 28 params
    /// types would be 28 chances to forget one, and a connection handshake or header frame would add a
    /// second message shape beside the request. Additive and `#[serde(default)]`, so every client that predates it
    /// stays valid on the wire.
    ///
    /// **Absent is not a grant.** What a call presenting no credential resolves to is decided by
    /// the peer-classification table, and it is not `Operator`. The agent token is deliberately *not* here: it stays a
    /// per-call param on the agent-callable methods, such as [`agent::AskParams::token`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cred: Option<cred::OperatorCredential>,
}

/// The daemon's answer to a [`Request`]. Exactly one of `result` and `error` is present.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The request's id.
    pub id: RequestId,
    /// The method's result, on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error, on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A success response.
    #[must_use]
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response.
    #[must_use]
    pub fn err(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A message from the daemon with no id and no reply: subscriptions and streams.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// A [`NotificationKind`] name.
    pub method: String,
    /// The notification's params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    /// An [`ErrorCode`] value, [`JOURNAL_STOPPED_CODE`], or a standard JSON-RPC code.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Typed detail by code, including [`JournalStoppedData`] for [`JOURNAL_STOPPED_CODE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A stopped journal refuses later mutations with this additive wire code.
/// The exhaustive [`ErrorCode`] enum remains compatible; this code carries [`JournalStoppedData`].
pub const JOURNAL_STOPPED_CODE: i32 = -32006;

/// The failed append position of a journal that refuses further writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JournalStoppedData {
    /// The attempted lsn of the append that failed after its write began.
    pub lsn: dudes_types::Lsn,
}

impl fmt::Display for JournalStoppedData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "journal stopped accepting writes at segment {} · byte {}; likely a full disk or an I/O error; free space and restart the daemon",
            self.lsn.segment(),
            self.lsn.offset()
        )
    }
}

/// Error codes. The standard JSON-RPC codes plus the daemon's own in the `-32001…-32005` range.
/// An integer on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(i32)]
pub enum ErrorCode {
    /// JSON-RPC: the frame was not valid JSON.
    ParseError = -32700,
    /// JSON-RPC: the envelope was malformed.
    InvalidRequest = -32600,
    /// JSON-RPC: no such method.
    MethodNotFound = -32601,
    /// JSON-RPC: the params are invalid: they do not match the method's schema, or they break a
    /// rule the method checks before any effect.
    InvalidParams = -32602,
    /// JSON-RPC: the daemon failed.
    InternalError = -32603,
    /// The peer may not call this method (wrong credential, or an agent token on an operator
    /// method).
    Unauthorized = -32001,
    /// A conditional append lost: `data` is [`ConflictData`].
    Conflict = -32002,
    /// No such agent, goal, card, mail or pty.
    NotFound = -32003,
    /// A gate refused: `data` is [`RefusedData`].
    Refused = -32004,
    /// The provider CLI failed: `data` is [`ProviderErrorData`].
    ProviderError = -32005,
}

impl ErrorCode {
    /// Every code represented by this enum.
    pub const ALL: [Self; 10] = [
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::InternalError,
        Self::Unauthorized,
        Self::Conflict,
        Self::NotFound,
        Self::Refused,
        Self::ProviderError,
    ];

    /// The numeric code.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// From a numeric code; `None` for a code this enum does not represent.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }

    /// What the code means, for the schema and for messages.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ParseError => "JSON-RPC: the frame was not valid JSON.",
            Self::InvalidRequest => "JSON-RPC: the envelope was malformed.",
            Self::MethodNotFound => "JSON-RPC: no such method.",
            Self::InvalidParams => {
                "JSON-RPC: the params are invalid: they do not match the method's schema, or they break a rule the method checks before any effect."
            }
            Self::InternalError => "JSON-RPC: the daemon failed.",
            Self::Unauthorized => "The peer may not call this method.",
            Self::Conflict => "A conditional append lost; `data` is ConflictData.",
            Self::NotFound => "No such agent, goal, card, mail or pty.",
            Self::Refused => "A gate refused; `data` is RefusedData.",
            Self::ProviderError => "The provider CLI failed; `data` is ProviderErrorData.",
        }
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i32(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = i32::deserialize(deserializer)?;
        Self::from_code(code)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown error code {code}")))
    }
}

impl JsonSchema for ErrorCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ErrorCode".into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        "dudes_proto::rpc::ErrorCode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let one_of: Vec<Value> = Self::ALL
            .iter()
            .map(|code| {
                serde_json::json!({
                    "const": code.code(),
                    "title": format!("{code:?}"),
                    "description": code.describe(),
                })
            })
            .collect();
        schemars::json_schema!({
            "type": "integer",
            "description": "JSON-RPC error code: the standard codes plus -32001 Unauthorized, -32002 Conflict, -32003 NotFound, -32004 Refused, -32005 ProviderError.",
            "oneOf": one_of,
        })
    }
}

/// `data` of a [`ErrorCode::Conflict`]: the sequence the caller should have used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConflictData {
    /// The card's current `seq`.
    pub current_seq: Seq,
}

/// `data` of a [`ErrorCode::Refused`]: which gate said no.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RefusedData {
    /// The gate.
    pub gate: RefusalGate,
}

/// Why a call was refused.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RefusalGate {
    /// A budget ceiling is reached.
    Budget,
    /// The hop cap is reached.
    Hops,
    /// The agent is halted.
    Halted,
    /// The agent is `Exhausted`.
    Exhausted,
    /// The provider is managed-only; `agent.attach` is not possible.
    ManagedOnly,
    /// The goal has an open turn; `goal.reassign` must wait. `goal.set` refuses with it while the
    /// agent has a turn or a check running in its directory, where the new goal's check would run.
    OpenTurn,
    /// The call presented a token matching no minting, or a retired agent's.
    Token,
    /// `agent.retire` was called on an agent whose open turn has `Unknown`
    /// liveness (the retirement cannot wait, so the call is
    /// refused rather than deferred).
    LivenessUnknown,
    // Appended rather than filed next to `OpenTurn`, which reads better: this enum derives `Ord`,
    // so inserting mid-list renumbers every variant below it — measured as a second
    // `cargo semver-checks` major break (`partial_ord_enum_variants_reordered`, naming `Token`
    // and `LivenessUnknown`) on top of the unavoidable `enum_variant_added`. Same reason
    // `LivenessUnknown` is where it is.
    //
    // Where the gate lives: **not in the fold.** The obvious home for the
    // one-open-question gate is the `Record::HumanQuestion` arm of the supervisor's `World::apply`, which sets
    // `open_question` unconditionally — and that arm cannot refuse anything. It runs on an entry
    // that has *already* reached the disk, so by the time it sees a second question the second
    // question is durable, and all a projection could decline is to **display** a record the
    // journal holds (invariant 1). The arm stays unconditional on purpose: it must fold whatever
    // the log actually contains, including a second question written by a daemon built before the
    // gate existed. So the handler reads the open question *before* it constructs the record —
    // and a test of the gate asserts on the records the daemon **asked** to have written, never
    // on the projection's final state, because the projection reads the same under both shapes.
    /// The agent already has an open question; a second `agent.ask` is refused. A question is open from its `Record::HumanQuestion` until a `Record::HumanAnswer`
    /// for the same agent, and an agent has one slot for one — so a second append would silently
    /// overwrite a question a human may be reading. The refusal is therefore a gate in front of
    /// the append, never a correction applied after it.
    ///
    /// **What refuses with this is `agent.ask`** (an agent-only method), in the daemon's `Ask::Ask`
    /// arm: the gate reads the projection's `open_question` on the loop thread, before the
    /// command reaches `persist`, which is the pre-append moment the paragraph above requires.
    OpenQuestion,
    /// A well-formed conditioned effect was suppressed or quarantined after its method's `Requirement` admitted the call.
    ConditionedEffect,
    // Appended, not filed beside `Budget` where it reads better, for the reason the comment above
    // `OpenQuestion` gives and does not need repeating: `Ord` plus a mid-list insertion is a
    // second `cargo semver-checks` major on top of the unavoidable `enum_variant_added`.
    /// A ceiling is **required** for this call and none is set.
    ///
    /// The daemon's own environment carries an environment variable that means a turn on the named
    /// provider can be billed, and the call set no ceiling in any scope or unit — so a managed
    /// child would inherit that credential and run with nothing to stop it but the hop cap. The
    /// refusal is made before the record is written, so a refused `agent.hire` or `goal.set`
    /// journals nothing.
    ///
    /// **This is the opposite of [`RefusalGate::Budget`], which is why it is not that value.**
    /// `Budget` means *a ceiling is reached*; this means *no ceiling exists*. A caller branching on
    /// one must never be handed the other. It is not a configuration complaint either: the
    /// configuration can express a ceiling perfectly well and the operator did not give one, and a
    /// missing instruction is not an inexpressible one.
    ///
    /// The remedy is any ceiling in any scope — `--usd-per-day`, `--usd`, `--tokens`, or
    /// `[budget] floor_usd_per_day`. **A ceiling of `0` counts**: it is a ceiling the operator set,
    /// and it refuses at the first turn rather than at the hire.
    CeilingRequired,
    /// A ceiling exists in a unit whose configured per-turn reserve is zero.
    ///
    /// Appended so every earlier variant keeps its derived order and serialized spelling.
    ReserveFloorRequired,
    /// The call names no question that is open for that agent: a local `agent.answer`
    /// to an agent with no open question, refused before any record is constructed.
    ///
    /// **The opposite of [`RefusalGate::OpenQuestion`]**, which means the agent already has one.
    /// The same spelling is the Floor answer route's reason for the same fact. Appended, for the
    /// reason given above `OpenQuestion`.
    QuestionClosed,
    /// A hire's working directory is at, under or above a worktree that is leased to a live agent
    /// or is still being created, or it cannot be placed against the leases. Refused
    /// before any record is constructed. `agent.hire` answers with this gate. The Floor's
    /// onboarding hire is refused as well, with its usual empty 403 and no reason. Appended, for
    /// the reason given above `OpenQuestion`.
    WorktreeOverlap,
    /// A worktree hire cannot be served: this platform or git does not support it, the directory
    /// is no repository, a named lease is unknown or from another repository, or the daemon could
    /// not create the tree. Nothing is left behind and nothing is journaled as an
    /// agent. Appended, for the reason given above `OpenQuestion`.
    WorktreeUnavailable,
    /// `goal.set` ran the goal's check once before the first turn and it already passed, so it
    /// cannot show when this goal is done. Nothing is journaled. Appended, for the reason given
    /// above `OpenQuestion`.
    CheckAlreadyPasses,
    /// `goal.set` ran the goal's check once before the first turn and it timed out or was ended
    /// by a signal, so it shows nothing about the state before work. Nothing is journaled.
    CheckDidNotFinish,
    /// `goal.set` could not run the goal's check before the first turn: its directory or program
    /// is missing, the program is inside the tree, or the board card does not exist. Nothing is
    /// journaled.
    CheckCannotRun,
}

/// `data` of a [`ErrorCode::ProviderError`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderErrorData {
    /// Which provider.
    pub provider: Provider,
    /// What it said.
    pub detail: String,
}

/// No params, or an `ok` result.
///
/// Strict in both roles, which the module doc's "results are not strict" edge does not
/// contradict: a field added here would appear on every method that answers `ok` at once, so
/// this is the one shape that cannot grow one, and denying takes nothing from it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Params naming one agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRef {
    /// The agent.
    pub agent: AgentId,
}

/// Params naming one agent and carrying text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentText {
    /// The agent.
    pub agent: AgentId,
    /// The text.
    pub text: String,
}

/// One row of the method table: a method's params and result types. Schema-only; not a wire
/// shape.
#[derive(JsonSchema)]
#[schemars(inline)]
pub struct MethodShape<P, R> {
    /// The params object.
    pub params: P,
    /// The result.
    pub result: R,
}

/// Text that names no method or notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnknownName;

impl fmt::Display for UnknownName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown method or notification name")
    }
}

impl std::error::Error for UnknownName {}

macro_rules! names {
    (
        $(#[$enum_doc:meta])* $enum_name:ident,
        $(#[$table_doc:meta])* $table_name:ident,
        $row:ident;
        $( $(#[$doc:meta])* $name:literal => $variant:ident / $field:ident : $($shape:ty)=>+ ; )*
    ) => {
        $(#[$enum_doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        pub enum $enum_name {
            $( $(#[$doc])* #[serde(rename = $name)] $variant, )*
        }

        impl $enum_name {
            /// Every entry, in table order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )* ];

            /// The wire name.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $( Self::$variant => $name, )* }
            }
        }

        impl fmt::Display for $enum_name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.name())
            }
        }

        impl FromStr for $enum_name {
            type Err = UnknownName;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::ALL.iter().copied().find(|m| m.name() == text).ok_or(UnknownName)
            }
        }

        $(#[$table_doc])*
        #[derive(JsonSchema)]
        pub struct $table_name {
            $( $(#[$doc])* #[serde(rename = $name)] pub $field: $row<$($shape),+>, )*
        }
    };
}

names! {
    /// Every method name.
    Method,
    /// The method table: one property per method pairing its params with its result.
    /// Schema-only; not a wire shape.
    Methods,
    MethodShape;

    /// Liveness: version, socket path and journal lsn.
    "system.ping" => SystemPing / system_ping : Empty => system::PingResult;
    /// Everything `ping` says plus a provider probe table and per-agent health.
    "system.doctor" => SystemDoctor / system_doctor : Empty => system::DoctorResult;
    /// Stop issuing turns, wait for running ones, flush and exit.
    "system.shutdown" => SystemShutdown / system_shutdown : Empty => system::ShutdownResult;
    /// Start the closing-time protocol.
    "system.closingTime" => SystemClosingTime / system_closing_time : Empty => Empty;
    /// Hire a managed agent.
    "agent.hire" => AgentHire / agent_hire : agent::HireParams => agent::HireResult;
    /// Every hired, unretired agent.
    "agent.list" => AgentList / agent_list : Empty => Vec<crate::projection::AgentView>;
    /// One agent.
    "agent.get" => AgentGet / agent_get : AgentRef => crate::projection::AgentView;
    /// Retire an agent; its children are killed.
    "agent.retire" => AgentRetire / agent_retire : agent::RetireParams => Empty;
    /// Inject context on the agent's next event.
    "agent.steer" => AgentSteer / agent_steer : AgentText => Empty;
    /// Answer an open `HumanQuestion`.
    "agent.answer" => AgentAnswer / agent_answer : AgentText => Empty;
    /// Take the keyboard: opens the vendor TUI in a PTY and streams `pty.data`. Refused with
    /// `managed_only`, `exhausted` or `halted`.
    "agent.attach" => AgentAttach / agent_attach : agent::AttachParams => agent::AttachResult;
    /// Give the keyboard back; the loop re-decides.
    "agent.detach" => AgentDetach / agent_detach : AgentRef => Empty;
    /// The last `LoopDecided` with its gate trace.
    "agent.why" => AgentWhy / agent_why : AgentRef => agent::WhyResult;
    /// Give an agent a goal.
    "goal.set" => GoalSet / goal_set : goal::GoalSetParams => goal::GoalSetResult;
    /// Clear an agent's goal.
    "goal.clear" => GoalClear / goal_clear : AgentRef => Empty;
    /// Hand a goal to another agent; refused with `open_turn` while a turn is in flight.
    "goal.reassign" => GoalReassign / goal_reassign : goal::GoalReassignParams => Empty;
    /// Stop issuing turns for an agent; hooks and mail keep working.
    "loop.pause" => LoopPause / loop_pause : AgentRef => Empty;
    /// Lift a pause.
    "loop.resume" => LoopResume / loop_resume : AgentRef => Empty;
    /// Stop the agent: no turns, and a running one is ended at its next boundary.
    "loop.halt" => LoopHalt / loop_halt : AgentRef => Empty;
    /// The attacher's keystrokes, relayed verbatim to the PTY. Only the attacher may call it.
    "pty.write" => PtyWrite / pty_write : pty::PtyWriteParams => Empty;
    /// Resize the PTY.
    "pty.resize" => PtyResize / pty_resize : pty::PtyResizeParams => Empty;
    /// Create a card.
    "board.create" => BoardCreate / board_create : board::CardCreateParams => board::CardCreateResult;
    /// Claim a card; `conflict` when `expected_seq` is stale.
    "board.claim" => BoardClaim / board_claim : board::CardClaimParams => Empty;
    /// Move a card; `conflict` when `expected_seq` is stale.
    "board.transition" => BoardTransition / board_transition : board::CardTransitionParams => Empty;
    /// Cards, filtered.
    "board.list" => BoardList / board_list : board::CardListParams => Vec<crate::projection::CardView>;
    /// Send mail as the operator (`from` is `human`).
    "mail.send" => MailSend / mail_send : mail::MailSendParams => mail::MailSendResult;
    /// Append to the calling agent's memory (agent token).
    "memory.append" => MemoryAppend / memory_append : memory::MemoryAppendParams => memory::MemoryAppendResult;
    /// Read memory from an lsn.
    "memory.read" => MemoryRead / memory_read : memory::MemoryReadParams => memory::MemoryReadResult;
    /// A vendor hook event, verbatim, with the agent's token over the local transport.
    "hook.event" => HookEvent / hook_event : hook::HookEventParams => crate::reply::VendorReply;
    /// Subscribe to records from an lsn; each arrives as `journal.appended`.
    "journal.tail" => JournalTail / journal_tail : journal::JournalTailParams => Empty;
    /// Rebuild every projection from the journal.
    "journal.replay" => JournalReplay / journal_replay : Empty => Empty;
    /// The current projection, then `floor.delta` notifications.
    "floor.subscribe" => FloorSubscribe / floor_subscribe : Empty => crate::projection::FloorProjection;
    /// Stop `floor.delta` notifications.
    "floor.unsubscribe" => FloorUnsubscribe / floor_unsubscribe : Empty => Empty;
    /// A managed agent asks its human something; the loop holds it at gate 3 until `agent.answer`.
    /// Agent-only, and the caller is the token's agent, never a params field.
    //
    // Appended rather than filed beside `agent.answer`, which reads better. Measured with
    // `cargo semver-checks` against the prior release: **appended, 2 major findings** —
    // `enum_variant_added` and
    // `constructible_struct_adds_field` on `Methods.agent_ask`, both unavoidable for any new row.
    // **Filed beside `agent.answer`, 3** — the same two plus
    // `enum_no_repr_variant_discriminant_changed`, naming the 23 variants below it. So the cost of
    // a mid-list row is a third major break and a renumbering of two thirds of the table.
    // Appending also makes the variant's position its method-table row, so `agent.ask` is row 34.
    "agent.ask" => AgentAsk / agent_ask : agent::AskParams => Empty;
    /// Issue a bounded, single-use bootstrap bearer for the read-only Floor.
    // Appended so the variant's position remains its method-table row, 35. Operator-only and
    // non-conditioned.
    "floor.session.issue" => FloorSessionIssue / floor_session_issue : Empty => floor::SessionIssueResult;
    /// Make the daemon adopt the `[floor.onboarding]` workspaces now in its own operator file,
    /// only when the file and the change are the caller's.
    // Appended so the variant's position remains its method-table row, 36. Operator-only.
    "floor.enrollment.reload" => FloorEnrollmentReload / floor_enrollment_reload : floor::EnrollmentReloadParams => floor::EnrollmentReloadResult;
    /// Change only a live agent's display name.
    "agent.rename" => AgentRename / agent_rename : agent::RenameParams => Empty;
    /// Change a live agent's role and authored appearance.
    "agent.look" => AgentLook / agent_look : agent::LookParams => Empty;
    /// Public colleagues. Agent calls present `team::RosterAgentParams`; business input is empty.
    "team.roster" => TeamRoster / team_roster : Empty => Vec<team::RosterEntry>;
    /// Replace the current goal and completion check without adding allowance.
    "goal.replace" => GoalReplace / goal_replace : goal::GoalReplaceParams => goal::GoalReplaceResult;
    /// Every worktree lease that is not removed, with its path. Operator-only.
    "worktree.list" => WorktreeList / worktree_list : Empty => Vec<worktree::WorktreeEntry>;
    /// The operator removed a released lease's tree: journal it, refusing while the path exists.
    /// Operator-only.
    "worktree.removed" => WorktreeRemoved / worktree_removed : worktree::RemovedParams => Empty;
    /// Bounded literal search of valid original memory and the current summary.
    "memory.search" => MemorySearch / memory_search : memory::MemorySearchParams => memory::MemorySearchResult;
    /// Read or replace one live agent's desired MCP names for later managed launches.
    "agent.mcp" => AgentMcp / agent_mcp : agent::McpParams => agent::McpResult;
    /// Make a ticket: a board card with a note, a project and a completion check. Operator-only.
    "ticket.create" => TicketCreate / ticket_create : ticket::TicketCreateParams => ticket::TicketCreateResult;
    /// Change a ticket's title, note or check while it is in To do. Operator-only.
    "ticket.edit" => TicketEdit / ticket_edit : ticket::TicketEditParams => Empty;
    /// Put a ticket in an agent's queue. Operator-only.
    "ticket.assign" => TicketAssign / ticket_assign : ticket::TicketAssignParams => ticket::TicketAssignResult;
    /// Take a ticket off the board. Operator-only.
    "ticket.remove" => TicketRemove / ticket_remove : ticket::TicketRemoveParams => Empty;
    /// One ticket with its note, check and queue place. Operator-only.
    "ticket.read" => TicketRead / ticket_read : ticket::TicketReadParams => ticket::TicketReadResult;
    /// Accept a ticket that is Ready for you as done. Operator-only.
    "ticket.accept" => TicketAccept / ticket_accept : ticket::TicketAcceptParams => Empty;
}

impl From<crate::record::ConditionedMethod> for Method {
    fn from(method: crate::record::ConditionedMethod) -> Self {
        match method {
            crate::record::ConditionedMethod::AgentSteer => Self::AgentSteer,
            crate::record::ConditionedMethod::AgentAnswer => Self::AgentAnswer,
            crate::record::ConditionedMethod::AgentAttach => Self::AgentAttach,
            crate::record::ConditionedMethod::AgentDetach => Self::AgentDetach,
            crate::record::ConditionedMethod::PtyWrite => Self::PtyWrite,
            crate::record::ConditionedMethod::MailSend => Self::MailSend,
            crate::record::ConditionedMethod::MemoryAppend => Self::MemoryAppend,
        }
    }
}

impl TryFrom<Method> for crate::record::ConditionedMethod {
    type Error = UnknownName;

    fn try_from(method: Method) -> Result<Self, Self::Error> {
        match method {
            Method::AgentSteer => Ok(Self::AgentSteer),
            Method::AgentAnswer => Ok(Self::AgentAnswer),
            Method::AgentAttach => Ok(Self::AgentAttach),
            Method::AgentDetach => Ok(Self::AgentDetach),
            Method::PtyWrite => Ok(Self::PtyWrite),
            Method::MailSend => Ok(Self::MailSend),
            Method::MemoryAppend => Ok(Self::MemoryAppend),
            _ => Err(UnknownName),
        }
    }
}

/// One row of the notification table. Schema-only; not a wire shape.
#[derive(JsonSchema)]
#[schemars(inline)]
pub struct NotificationShape<P> {
    /// The params object.
    pub params: P,
}

names! {
    /// Every notification name.
    NotificationKind,
    /// The notification table: one property per notification naming its params. Schema-only.
    Notifications,
    NotificationShape;

    /// What changed on the floor.
    "floor.delta" => FloorDelta / floor_delta : crate::projection::FloorDelta;
    /// The per-connection queue overflowed; discard the store and re-subscribe.
    "floor.resync" => FloorResync / floor_resync : Empty;
    /// Bytes from an attached PTY.
    "pty.data" => PtyData / pty_data : pty::PtyDataParams;
    /// An attached PTY exited.
    "pty.exit" => PtyExit / pty_exit : pty::PtyExitParams;
    /// An agent asked a human something.
    "agent.question" => AgentQuestion / agent_question : AgentText;
    /// A record was appended (for `journal.tail` subscribers).
    "journal.appended" => JournalAppended / journal_appended : journal::JournalAppendedParams;
}

/// The root of `schemas/rpc.schema.json`: envelopes, error codes and both tables.
/// Schema-only; not a wire shape.
#[derive(JsonSchema)]
pub struct Rpc {
    /// The request envelope.
    pub request: Request,
    /// The response envelope.
    pub response: Response,
    /// The notification envelope.
    pub notification: Notification,
    /// Error codes by name.
    pub error_code: ErrorCode,
    /// Typed `error.data` shapes.
    pub error_data: ErrorData,
    /// Every method.
    pub methods: Methods,
    /// Every notification.
    pub notifications: Notifications,
}

/// The typed `data` shapes, keyed by the code that carries them. Schema-only.
#[derive(JsonSchema)]
pub struct ErrorData {
    /// `-32002`.
    pub conflict: ConflictData,
    /// `-32004`.
    pub refused: RefusedData,
    /// `-32005`.
    pub provider_error: ProviderErrorData,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_edit_is_an_ordinary_method_and_not_a_browser_conditioned_effect() {
        let method = "agent.mcp".parse::<Method>().unwrap();
        assert_eq!(method.name(), "agent.mcp");
        assert!(crate::record::ConditionedMethod::try_from(method).is_err());
    }

    /// The error codes are the five of the RPC contract plus the JSON-RPC standard five,
    /// numerically distinct and invertible.
    #[test]
    fn error_codes_match_the_design() {
        assert_eq!(ErrorCode::Unauthorized.code(), -32001);
        assert_eq!(ErrorCode::Conflict.code(), -32002);
        assert_eq!(ErrorCode::NotFound.code(), -32003);
        assert_eq!(ErrorCode::Refused.code(), -32004);
        assert_eq!(ErrorCode::ProviderError.code(), -32005);
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.code()), Some(code));
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                Value::from(code.code()),
                "an integer on the wire"
            );
        }
        assert_eq!(ErrorCode::from_code(0), None);
        assert!(serde_json::from_str::<ErrorCode>("0").is_err());
    }

    /// Method names are unique, parse back to themselves, and every name in the method table
    /// is present.
    #[test]
    fn method_names_round_trip() {
        let mut names: Vec<&str> = Method::ALL.iter().map(|m| m.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Method::ALL.len());
        for method in Method::ALL {
            assert_eq!(method.name().parse::<Method>().unwrap(), *method);
            assert_eq!(
                serde_json::to_value(method).unwrap(),
                Value::String(method.name().to_owned())
            );
        }
        for expected in [
            "system.ping",
            "system.doctor",
            "system.shutdown",
            "system.closingTime",
            "agent.hire",
            "agent.list",
            "agent.get",
            "agent.retire",
            "goal.set",
            "goal.clear",
            "goal.reassign",
            "loop.pause",
            "loop.resume",
            "loop.halt",
            "agent.steer",
            "agent.answer",
            "agent.attach",
            "agent.detach",
            "pty.write",
            "pty.resize",
            "board.create",
            "board.claim",
            "board.transition",
            "board.list",
            "mail.send",
            "memory.append",
            "memory.read",
            "hook.event",
            "journal.tail",
            "journal.replay",
            "agent.why",
            "floor.subscribe",
            "floor.unsubscribe",
            "agent.ask",
            "floor.session.issue",
            "floor.enrollment.reload",
        ] {
            assert!(expected.parse::<Method>().is_ok(), "{expected} missing");
        }
        assert_eq!("nope".parse::<Method>(), Err(UnknownName));
        assert_eq!(
            "journal.appended".parse::<NotificationKind>().unwrap(),
            NotificationKind::JournalAppended
        );
    }

    #[test]
    fn the_method_table_keeps_old_rows_and_appends_tickets() {
        assert_eq!(
            Method::ALL.len(),
            50,
            "agent.mcp stays at row 44; the ticket methods append rows 45 to 50"
        );
        assert_eq!(Method::ALL[44], Method::TicketCreate);
        assert_eq!(Method::ALL[48], Method::TicketRead);
        assert_eq!(Method::ALL[49], Method::TicketAccept);
        assert_eq!(
            "floor.enrollment.reload".parse::<Method>(),
            Ok(Method::FloorEnrollmentReload)
        );
        assert_eq!(
            "floor.enrollment.reloads".parse::<Method>(),
            Err(UnknownName),
            "control: a name one character off does not parse"
        );
        assert_eq!(
            Method::ALL[34],
            Method::FloorSessionIssue,
            "row 35 keeps its position"
        );
        assert_eq!(
            Method::ALL[35],
            Method::FloorEnrollmentReload,
            "row 36 keeps its position"
        );
        assert_eq!(Method::ALL[36], Method::AgentRename);
        assert_eq!(Method::ALL[37], Method::AgentLook);
        assert_eq!(Method::ALL[38], Method::TeamRoster);
        assert_eq!(Method::ALL[39], Method::GoalReplace);
        assert_eq!(Method::ALL[40], Method::WorktreeList);
        assert_eq!(Method::ALL[41], Method::WorktreeRemoved);
        assert_eq!(Method::ALL[42], Method::MemorySearch);
        assert_eq!("memory.search".parse::<Method>(), Ok(Method::MemorySearch));
        assert_eq!(Method::ALL[43], Method::AgentMcp);
    }

    /// A `goal.set` request parses as an envelope when its `usd` is written in micro-dollars
    /// (`15000000`, not `15.0`), and a refusal response carries the typed `data`.
    #[test]
    fn request_envelope_parses_a_goal_set_with_integer_usd() {
        let text = r#"{"jsonrpc":"2.0","id":7,"method":"goal.set","params":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","text":"All tests green","predicate":{"command":{"argv":["npm","test"],"timeout_s":600}},"budget":{"usd":15000000},"hop_cap":40}}"#;
        let request: Request = serde_json::from_str(text).unwrap();
        assert_eq!(request.id, RequestId::Number(7));
        assert_eq!(request.method.parse::<Method>().unwrap(), Method::GoalSet);
        let params: goal::GoalSetParams = serde_json::from_value(request.params.unwrap()).unwrap();
        assert_eq!(params.hop_cap, 40);
        let response = Response::err(
            RequestId::Number(7),
            RpcError {
                code: ErrorCode::Refused.code(),
                message: "open turn".into(),
                data: Some(
                    serde_json::to_value(RefusedData {
                        gate: RefusalGate::OpenTurn,
                    })
                    .unwrap(),
                ),
            },
        );
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["error"]["code"], -32004);
        assert_eq!(json["error"]["data"]["gate"], "open_turn");
        assert!(json.get("result").is_none());
    }

    /// The credential field is **additive**: an envelope written before it existed still parses,
    /// and parses to "presented nothing" rather than to a default that means something.
    ///
    /// The frame below is the documented `goal.set` example with no `cred` key at all — the shape
    /// every client in the tree sends today.
    #[test]
    fn an_envelope_with_no_credential_still_parses_and_presents_nothing() {
        let text = r#"{"jsonrpc":"2.0","id":7,"method":"system.ping"}"#;
        let request: Request = serde_json::from_str(text).unwrap();
        assert!(
            request.cred.is_none(),
            "an absent credential is absent, not a value"
        );
        assert_eq!(
            request.id,
            RequestId::Number(7),
            "positive control: the rest of the envelope parsed"
        );
    }

    /// A credential on the envelope survives the round trip, and an envelope carrying none does
    /// not write the key — so a client that never presents one sends the bytes it sends today.
    #[test]
    fn a_credential_rides_the_envelope_and_absence_writes_no_key() {
        let bare = Request {
            jsonrpc: JSONRPC.to_owned(),
            id: RequestId::Number(1),
            method: Method::SystemPing.name().to_owned(),
            params: None,
            v: None,
            cred: None,
        };
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("cred").is_none(), "absence writes no key");

        let text = "K".repeat(cred::OPERATOR_CREDENTIAL_LEN);
        let presenting = Request {
            cred: Some(cred::OperatorCredential::new(&text).unwrap()),
            ..bare
        };
        let json = serde_json::to_value(&presenting).unwrap();
        assert_eq!(json["cred"], text, "and presence writes the credential");
        let back: Request = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.cred.as_ref().map(cred::OperatorCredential::expose),
            Some(text.as_str())
        );
    }

    /// A malformed credential is refused where the envelope is parsed, not carried inward as a
    /// value some later check has to remember to reject.
    #[test]
    fn a_malformed_credential_fails_the_envelope() {
        let text = r#"{"jsonrpc":"2.0","id":1,"method":"system.ping","cred":"too-short"}"#;
        assert!(serde_json::from_str::<Request>(text).is_err());
        let control = r#"{"jsonrpc":"2.0","id":1,"method":"system.ping","cred":"KKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKK"}"#;
        assert!(
            serde_json::from_str::<Request>(control).is_ok(),
            "positive control: a well-formed credential parses, so the refusal above is about \
             the value and not about the key"
        );
    }

    /// The peer-classification table refuses a call presenting "a token matching no minting, or a
    /// retired agent's" with `-32004 Refused{gate: token}` in all four provenance columns. The
    /// wire contract must be able to *say* that: the variant survives a round trip unchanged and
    /// spells itself `token`, so the daemon that sends the row and the client that reads it agree
    /// on one value. Without this, an accepted record mandates a refusal the type cannot express.
    #[test]
    fn refusal_gate_token_round_trips() {
        let refused = RefusedData {
            gate: RefusalGate::Token,
        };
        let json = serde_json::to_value(refused).unwrap();
        assert_eq!(
            json["gate"], "token",
            "the table's cell reads `gate: token`"
        );
        let back: RefusedData = serde_json::from_value(json).unwrap();
        assert_eq!(
            back, refused,
            "the round trip returns the value it started from"
        );
        assert_eq!(back.gate, RefusalGate::Token);
    }

    /// A daemon must refuse `agent.retire` on an agent whose open turn has `Unknown`
    /// liveness with `-32004 Refused{gate: \"liveness_unknown\"}` — the one refusal
    /// in that record that cannot be deferred, because a half-retired agent is a state the
    /// journal cannot name. The wire contract must be able to *say* it: the variant survives a
    /// round trip unchanged and spells itself `liveness_unknown` through `RefusedData`, so the
    /// daemon that refuses the call and the client that reads the refusal agree on one value.
    ///
    /// The spelling assertion is reached through `RefusedData`, not by matching the string
    /// anywhere: `KillDeferredReason::LivenessUnknown` already serialises to the same word
    /// (`record.rs`), and a check that merely found `liveness_unknown` would be green before
    /// this variant existed.
    #[test]
    fn refusal_gate_liveness_unknown_round_trips() {
        let refused = RefusedData {
            gate: RefusalGate::LivenessUnknown,
        };
        let json = serde_json::to_value(refused).unwrap();
        assert_eq!(
            json["gate"], "liveness_unknown",
            "`agent.retire` on an unknown-liveness turn is refused with `gate: \"liveness_unknown\"`"
        );
        let back: RefusedData = serde_json::from_value(json).unwrap();
        assert_eq!(
            back, refused,
            "the round trip returns the value it started from"
        );
        assert_eq!(back.gate, RefusalGate::LivenessUnknown);
    }

    /// A second `agent.ask` from an agent whose question is still open is refused
    /// with `-32004 Refused{gate: open_question}`. The wire contract must be able to *say* it:
    /// the variant survives a round trip unchanged and spells itself `open_question`, so the
    /// daemon that refuses the call and the client that reads the refusal agree on one value.
    ///
    /// The spelling assertion is reached through `RefusedData` rather than by matching the string
    /// anywhere: `open_question` is already a field name on the floor's agent view and a word in
    /// several doc comments, and a check that merely found it would have been green before this
    /// variant existed.
    #[test]
    fn refusal_gate_open_question_round_trips() {
        let refused = RefusedData {
            gate: RefusalGate::OpenQuestion,
        };
        let json = serde_json::to_value(refused).unwrap();
        assert_eq!(
            json["gate"], "open_question",
            "a second `agent.ask` refuses with `gate: \"open_question\"`"
        );
        let back: RefusedData = serde_json::from_value(json).unwrap();
        assert_eq!(
            back, refused,
            "the round trip returns the value it started from"
        );
        assert_eq!(back.gate, RefusalGate::OpenQuestion);
    }

    #[test]
    fn refusal_gate_reserve_floor_required_round_trips() {
        let refused = RefusedData {
            gate: RefusalGate::ReserveFloorRequired,
        };
        let json = serde_json::to_value(refused).unwrap();
        assert_eq!(json["gate"], "reserve_floor_required");
        let back: RefusedData = serde_json::from_value(json).unwrap();
        assert_eq!(back, refused);
    }

    /// the local answer's no-open-question refusal spells itself `question_closed` and
    /// is ordered after every earlier gate, so no existing value moved.
    #[test]
    fn refusal_gate_question_closed_round_trips_and_is_appended() {
        let refused = RefusedData {
            gate: RefusalGate::QuestionClosed,
        };
        let json = serde_json::to_value(refused).unwrap();
        assert_eq!(json["gate"], "question_closed");
        let back: RefusedData = serde_json::from_value(json).unwrap();
        assert_eq!(back, refused);
        assert!(RefusalGate::ReserveFloorRequired < RefusalGate::QuestionClosed);
        assert_ne!(RefusalGate::QuestionClosed, RefusalGate::OpenQuestion);
    }

    /// What `RefusalGate` *refuses* to read. The variants it accepts are pinned one at a time
    /// above; the boundary of that set is not, and a `#[serde(alias = "…")]` widens it in a
    /// direction no schema check looks — the published document would keep listing the gates it
    /// always listed while the daemon quietly accepted another word, so a client and the daemon
    /// would disagree about what a refusal even is. The rejection is therefore the assertion.
    ///
    /// The control is a gate round-tripped through `RefusedData` rather than a literal, so it
    /// proves the refusal above is about the *value* and not about the key, the struct or a
    /// renamed field — and it names no spelling that could go stale. No count is asserted: any
    /// one gate is enough to prove the reader works.
    #[test]
    fn an_unknown_refusal_gate_is_refused() {
        let unknown = serde_json::json!({"gate": "definitely_not_a_gate"});
        assert!(
            serde_json::from_value::<RefusedData>(unknown.clone()).is_err(),
            "an unpublished gate must not deserialise"
        );
        let err = serde_json::from_value::<RefusedData>(unknown).unwrap_err();
        assert!(
            err.to_string().contains("unknown variant"),
            "refused as an unknown variant, not for some unrelated reason: {err}"
        );

        let known = serde_json::to_value(RefusedData {
            gate: RefusalGate::Budget,
        })
        .unwrap();
        assert!(
            serde_json::from_value::<RefusedData>(known).is_ok(),
            "positive control: a gate the type itself emits reads back, so the refusal above \
             is about the value and not about the shape around it"
        );
    }

    /// Contract v1 carries money as integer micro-dollars. The documented example's float
    /// `"usd": 15.0` is refused rather than rounded.
    #[test]
    fn float_usd_in_params_is_rejected() {
        let params = serde_json::json!({
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "text": "All tests green",
            "predicate": {"command": {"argv": ["npm", "test"], "timeout_s": 600}},
            "budget": {"usd": 15.0},
            "hop_cap": 40
        });
        let err = serde_json::from_value::<goal::GoalSetParams>(params).unwrap_err();
        assert!(
            err.to_string().contains("floating point `15.0`"),
            "refused as a float, not rounded: {err}"
        );
        assert!(
            serde_json::from_value::<crate::budget::AgentBudget>(
                serde_json::json!({"usd_per_day": "15"})
            )
            .is_err(),
            "a string is not money either"
        );
    }

    /// The scan behind [`every_method_params_type_refuses_unknown_fields`], as a function so
    /// that its own arms can be measured. Returns one line per offending method.
    ///
    /// It reads the *generated* method table rather than the source text, so it sees exactly
    /// what `schemas/rpc.schema.json` publishes: `deny_unknown_fields` is
    /// `additionalProperties: false` there, and "the attribute is on the struct" and "the
    /// published schema denies" are then one claim rather than two.
    ///
    /// A params shape that is not a `$ref` to a named object — an array, an inline schema, a
    /// dangling reference — is an **offender**, not a skip: nothing that is not a named object
    /// schema can carry the attribute, and a scan that skipped it would go quiet exactly where
    /// it stopped being able to see.
    ///
    /// **What it cannot see.** A params type whose `JsonSchema` is hand-written to emit
    /// `additionalProperties: false` while its serde derive stays permissive passes this green,
    /// and the wire still drops the key. That is not hypothetical: [`ErrorCode`] already carries
    /// a manual impl (`:268-294`), and any type may acquire one. The cover is
    /// [`every_params_type_refuses_a_misspelled_key`], which asks serde rather than schemars —
    /// the two scans are not redundant, and neither alone is the guarantee.
    fn method_params_admitting_unknown_fields(rpc: &Value) -> Vec<String> {
        let defs = &rpc["$defs"];
        let table = defs["Methods"]["properties"]
            .as_object()
            .expect("the method table is an object keyed by method name");
        let mut offenders = Vec::new();
        for (method, row) in table {
            let Some(reference) = row["properties"]["params"]["$ref"].as_str() else {
                offenders.push(format!("{method}: params is not a named object schema"));
                continue;
            };
            let name = reference.trim_start_matches("#/$defs/");
            if defs[name]["additionalProperties"] != Value::Bool(false) {
                offenders.push(format!("{method}: `{name}` admits unknown fields"));
            }
        }
        offenders
    }

    /// Every method's params type fails closed. An operator's misspelled key is refused, not
    /// dropped, so a setting that did nothing is distinguishable from one that was never read.
    ///
    /// The scan walks the generated method table, which the `names!` macro writes from the one
    /// list of methods — so a method added with a params struct that forgot the attribute
    /// reddens this without anyone having to remember the rule exists.
    #[test]
    fn every_method_params_type_refuses_unknown_fields() {
        let rpc = crate::schema::schema_for::<Rpc>().to_value();
        let offenders = method_params_admitting_unknown_fields(&rpc);
        assert!(
            offenders.is_empty(),
            "a params type must be #[serde(deny_unknown_fields)]: {offenders:#?}"
        );
        assert_eq!(
            rpc["$defs"]["Methods"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            Method::ALL.len(),
            "positive control: the scan walked every method, not an empty table"
        );
    }

    /// The scan's own arms, measured rather than assumed: a clean table must come back empty
    /// and each defect must come back named, or the emptiness above says nothing.
    #[test]
    fn the_params_scan_reports_a_type_that_admits_unknown_fields() {
        fn table(params: &Value, def: &Value) -> Value {
            serde_json::json!({
                "$defs": {
                    "Methods": {"properties": {"x.y": {"properties": {"params": params}}}},
                    "Subject": def,
                }
            })
        }
        let referenced = serde_json::json!({"$ref": "#/$defs/Subject"});
        let strict = serde_json::json!({"type": "object", "additionalProperties": false});

        assert!(
            method_params_admitting_unknown_fields(&table(&referenced, &strict)).is_empty(),
            "the arm that must stay green: a params type that denies is not an offender"
        );
        for (label, doc) in [
            (
                "no attribute at all",
                table(&referenced, &serde_json::json!({"type": "object"})),
            ),
            (
                "additionalProperties spelled true",
                table(
                    &referenced,
                    &serde_json::json!({"type": "object", "additionalProperties": true}),
                ),
            ),
            (
                "a params shape that is not a named object",
                table(&strict, &strict),
            ),
            (
                "a reference to nothing",
                table(&serde_json::json!({"$ref": "#/$defs/Absent"}), &strict),
            ),
        ] {
            assert_eq!(
                method_params_admitting_unknown_fields(&doc).len(),
                1,
                "{label}: the scan must name this"
            );
        }
    }

    /// One params type's three arms: the object it serialises to is accepted (the control), an
    /// added key is refused, and each of its own keys, misspelled in turn, is refused.
    ///
    /// The control is what makes the refusals mean anything — without it a decoder that refused
    /// every object would pass — and the misspelled arm is the operator's actual mistake, which
    /// the added-key arm alone does not reproduce.
    fn params_type_fails_closed<T>(label: &str, sample: &T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + fmt::Debug,
    {
        let good = serde_json::to_value(sample).expect("a params sample serialises");
        let object = good
            .as_object()
            .unwrap_or_else(|| panic!("{label}: params are a JSON object"))
            .clone();

        let back: T = serde_json::from_value(good.clone())
            .unwrap_or_else(|err| panic!("{label}: the control did not decode: {err}\n{good:#}"));
        assert_eq!(&back, sample, "{label}: the control round-trips");

        // What the decode said, whether it refused or — the failure this is looking for —
        // accepted and dropped the key.
        let outcome = |value: Value| {
            serde_json::from_value::<T>(value).map_or_else(
                |err| err.to_string(),
                |parsed| format!("accepted {parsed:?}"),
            )
        };

        let mut added = object.clone();
        added.insert("sandboxx".to_owned(), Value::Bool(true));
        let err = outcome(Value::Object(added));
        assert!(
            err.contains("unknown field"),
            "{label}: an added key must be refused as unknown, got {err}"
        );

        for key in object.keys() {
            let mut typo = object.clone();
            let value = typo.remove(key).expect("a key just read from the map");
            typo.insert(format!("{key}x"), value);
            let err = outcome(Value::Object(typo));
            assert!(
                err.contains("unknown field"),
                "{label}: `{key}` misspelled must be refused as unknown, got {err}"
            );
        }
    }

    /// Every params type in the method table, one misspelled key each, with its own control.
    ///
    /// The list is written out rather than derived: a type is reachable from the table only as
    /// a schema, and this test is about serde's behaviour on a value.
    /// [`every_method_params_type_refuses_unknown_fields`] is what makes a type added to the
    /// table and forgotten here still fail.
    #[test]
    #[allow(clippy::too_many_lines)] // one sample per params type; a table, not logic
    fn every_params_type_refuses_a_misspelled_key() {
        use dudes_types::{CardStatus, Lsn, Provider, Seq, Tokens, Usd};

        use crate::budget::{AgentBudget, GoalBudget};
        use crate::goal::Predicate;
        use crate::record::{MailAct, MailTo, samples};

        let agent = samples::agent();

        params_type_fails_closed("Empty", &Empty {});
        params_type_fails_closed("AgentRef", &AgentRef { agent });
        params_type_fails_closed(
            "AgentText",
            &AgentText {
                agent,
                text: "ship it".to_owned(),
            },
        );
        params_type_fails_closed(
            "HireParams",
            &agent::HireParams {
                character: None,
                accent: None,
                name: "Hector".to_owned(),
                role: "backend".to_owned(),
                department: "engineering".to_owned(),
                provider: Provider::Claude,
                cwd: "/repo".into(),
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_micros(5_000_000)),
                    tokens_per_day: Some(Tokens::new(1_000)),
                },
                sandbox: Some(true),
                isolation: None,
                max_turns: Some(3),
                max_budget: Some(Usd::from_micros(2_500_000)),
                trusted: false,
                checkout: None,
                from: None,
                mcp_servers: Vec::new(),
            },
        );
        params_type_fails_closed(
            "RenameParams",
            &agent::RenameParams {
                agent,
                name: "Kim".to_owned(),
            },
        );
        params_type_fails_closed(
            "LookParams",
            &agent::LookParams {
                agent,
                role: "designer".to_owned(),
                character: Some(dudes_types::CastCharacter::H),
                accent: None,
            },
        );
        params_type_fails_closed(
            "McpParams",
            &agent::McpParams {
                agent,
                mcp_servers: None,
            },
        );
        params_type_fails_closed(
            "RetireParams",
            &agent::RetireParams {
                agent,
                reason: "done".to_owned(),
            },
        );
        params_type_fails_closed(
            "AttachParams",
            &agent::AttachParams {
                agent,
                cols: 80,
                rows: 24,
            },
        );
        params_type_fails_closed(
            "GoalSetParams",
            &goal::GoalSetParams {
                agent,
                text: "all tests green".to_owned(),
                predicate: Predicate::Command {
                    argv: vec!["cargo".to_owned(), "test".to_owned()],
                    timeout_s: 600,
                },
                budget: GoalBudget {
                    usd: Some(Usd::from_micros(15_000_000)),
                    tokens: None,
                },
                hop_cap: 40,
                check: None,
                may_already_pass: false,
            },
        );
        params_type_fails_closed(
            "GoalReassignParams",
            &goal::GoalReassignParams {
                goal: samples::goal(),
                to: samples::other_agent(),
                reason: Some("handoff".to_owned()),
            },
        );
        params_type_fails_closed(
            "PtyWriteParams",
            &pty::PtyWriteParams {
                pty: pty::PtyId(1),
                data: "aGk=".to_owned(),
            },
        );
        params_type_fails_closed(
            "PtyResizeParams",
            &pty::PtyResizeParams {
                pty: pty::PtyId(1),
                cols: 80,
                rows: 24,
            },
        );
        params_type_fails_closed(
            "CardCreateParams",
            &board::CardCreateParams {
                title: "wire the door".to_owned(),
                department: "engineering".to_owned(),
                deps: vec![samples::card()],
            },
        );
        params_type_fails_closed(
            "CardClaimParams",
            &board::CardClaimParams {
                card: samples::card(),
                agent,
                expected_seq: Seq(3),
                token: None,
            },
        );
        params_type_fails_closed(
            "CardTransitionParams",
            &board::CardTransitionParams {
                card: samples::card(),
                to: CardStatus::Doing,
                expected_seq: Seq(3),
                token: None,
            },
        );
        params_type_fails_closed(
            "CardListParams",
            &board::CardListParams {
                department: Some("engineering".to_owned()),
                status: Some(CardStatus::Todo),
                claimed_by: Some(agent),
            },
        );
        params_type_fails_closed(
            "MailSendParams",
            &mail::MailSendParams {
                to: MailTo::Agent { agent },
                act: MailAct::Request,
                subject: "closing time".to_owned(),
                body: "wrap up".to_owned(),
            },
        );
        params_type_fails_closed(
            "MemoryAppendParams",
            &memory::MemoryAppendParams {
                agent: None,
                text: "learned something".to_owned(),
                token: None,
            },
        );
        params_type_fails_closed(
            "MemoryReadParams",
            &memory::MemoryReadParams {
                agent: Some(agent),
                since_lsn: Some(Lsn(7)),
                token: None,
            },
        );
        params_type_fails_closed(
            "MemorySearchParams",
            &memory::MemorySearchParams {
                query: "literal".into(),
                agent: Some(agent),
                since_lsn: Some(Lsn(7)),
                limit: Some(10),
                token: None,
            },
        );
        params_type_fails_closed(
            "JournalTailParams",
            &journal::JournalTailParams {
                from_lsn: Some(Lsn(7)),
            },
        );
        params_type_fails_closed(
            "HookEventParams",
            &hook::HookEventParams {
                token: hook::HookToken::new("a".repeat(hook::HOOK_TOKEN_LEN)).unwrap(),
                raw: serde_json::json!({"hook_event_name": "PreToolUse"}),
            },
        );
        params_type_fails_closed(
            "AskParams",
            &agent::AskParams {
                token: hook::HookToken::new("a".repeat(hook::HOOK_TOKEN_LEN)).unwrap(),
                text: "which branch?".to_owned(),
            },
        );
        params_type_fails_closed(
            "EnrollmentReloadParams",
            &floor::EnrollmentReloadParams {
                apply: Some(floor::EnrollmentApply {
                    sha256: "0".repeat(64),
                    added: vec!["repo_2".to_owned()],
                    removed: Vec::new(),
                    changed: Vec::new(),
                }),
            },
        );
    }

    /// `agent.ask` cannot be told who is asking: `AskParams` has no `agent` field, so a call that
    /// names one is refused `-32602` in the decode, before any handler sees it.
    ///
    /// **This is the runnable form of "the call carries no sender-supplied `agent`
    /// field"*.** The row is granted to `Agent` alone on the strength of the caller being the
    /// token's agent and nothing else; a params type that accepted and dropped an `agent` key
    /// would read identically at every call site and would leave the hole open for the first
    /// handler that later reached for it. `deny_unknown_fields` is what turns the sentence into a
    /// check, and the neighbours' [`AgentText`] — same method family, `agent` present — is what
    /// makes the mistake plausible enough to pin.
    ///
    /// The control is the identical object without the `agent` key, which decodes.
    #[test]
    fn agent_ask_params_refuse_a_sender_supplied_agent() {
        let token = "a".repeat(hook::HOOK_TOKEN_LEN);
        let with_agent = serde_json::json!({
            "token": token,
            "text": "which branch?",
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        });
        let err = serde_json::from_value::<agent::AskParams>(with_agent)
            .expect_err("agent is not a field of AskParams")
            .to_string();
        assert!(
            err.contains("unknown field `agent`"),
            "the asker is the resolved peer, never a params field: {err}"
        );

        let without = serde_json::json!({ "token": token, "text": "which branch?" });
        let parsed: agent::AskParams = serde_json::from_value(without)
            .expect("control: the same object without `agent` is a well-formed AskParams");
        assert_eq!(parsed.text, "which branch?");
    }

    /// The boundary, pinned so it is not closed by accident: the attribute stops at the params
    /// object and does **not** reach a type nested inside it.
    ///
    /// `AgentBudget` is also `Record::AgentHired`'s `budget`, so it is read back out of the
    /// journal as well as off the wire. Making it strict would turn a field added to a later
    /// contract — a *minor* change by this crate's own rule — into a frame an older reader
    /// refuses, which is a turn-outcome failure, and a daemon that cannot replay its own
    /// journal cannot start. The nested surface is a separate decision with a different blast
    /// radius, not a line to add here.
    ///
    /// **What the residual costs, so that "pinned" is not read as "harmless."** The dropped
    /// field is an *admission ceiling*, not a display value: `dudes_policy`'s `reached` asks
    /// `ceiling.tokens.is_some_and(|cap| spent.work_tokens >= cap)`, so a `tokens_per_day` that
    /// decodes to `None` is **uncapped**, not unset — one transposed letter hires an agent with
    /// no token ceiling and says nothing, which is invariant 4's territory. That is why the
    /// assertion below is on the decoded *value* with a correctly spelled control beside it,
    /// rather than on `is_ok()`: `is_ok()` cannot see which field went missing.
    ///
    /// The vendor payload inside `hook.event` is the same boundary reached from the other side,
    /// and its claim is not merely that the frame parses — it is that the cargo arrives
    /// **intact**, since the adapter reads fields out of `raw` that this crate has never heard
    /// of. So the second arm asserts the vendor's unknown key is still there afterwards.
    #[test]
    fn the_attribute_stops_at_the_params_object() {
        use dudes_types::{Tokens, Usd};

        use crate::budget::AgentBudget;

        let dropped: AgentBudget = serde_json::from_value(
            serde_json::json!({"usd_per_day": 5_000_000, "tokens_per_dayy": 1_000}),
        )
        .expect("a journal-shared nested type admits an unknown field");
        assert_eq!(
            dropped,
            AgentBudget {
                usd_per_day: Some(Usd::from_micros(5_000_000)),
                tokens_per_day: None,
            },
            "the misspelled ceiling is dropped to `None`, which `dudes_policy::budget::reached` \
             reads as no token ceiling at all"
        );

        // Control: the same object with the key spelled correctly keeps the ceiling, so the
        // assertion above is about the misspelling and not about a field that never arrives.
        let kept: AgentBudget = serde_json::from_value(
            serde_json::json!({"usd_per_day": 5_000_000, "tokens_per_day": 1_000}),
        )
        .expect("the correctly spelled ceiling");
        assert_eq!(kept.tokens_per_day, Some(Tokens::new(1_000)));

        let event: hook::HookEventParams = serde_json::from_value(serde_json::json!({
            "token": "a".repeat(hook::HOOK_TOKEN_LEN),
            "raw": {"hook_event_name": "PreToolUse", "a_field_the_vendor_added": 1},
        }))
        .expect("the vendor's own JSON is untouched by the params object's attribute");
        assert_eq!(
            event.raw,
            serde_json::json!({"hook_event_name": "PreToolUse", "a_field_the_vendor_added": 1}),
            "the cargo arrives intact, key the contract has never heard of included"
        );
    }
}
