//! The registered method surface: ten ordinary handlers and three conditioned effects over
//! injected ports.
//!
//! Nine ordinary methods require `Operator`. The tenth, `agent.ask`, is `Agent`-only. `agent.answer` and `agent.steer` are Operator
//! methods too, but the conditioned-effect router
//! additionally conditions their effect on authority and provenance.
//!
//! # Every shape comes from the contract crate
//!
//! No method here invents a request or a response type. `schemas/` is generated from
//! `dudes-proto` and diffed in CI, so a locally-defined result would pass review and fail the gate
//! — and, worse, would be a wire shape with no schema for the floor's client to read.
//!
//! # Three ports, and what the daemon owes them
//!
//! These ordinary handlers decide nothing, journal nothing and read no file: their behaviour
//! arrives as [`Control`] and [`JournalRead`], which the daemon implements. [`ConditionedAgentControl`]
//! is the persistence port for the two conditioned agent text effects. The separate
//! conditioned router owns their pure disposition and injected persistence seam. That is not only
//! layering hygiene — `dudes-journal` links `redb`, and this is the transport the hook shim links,
//! so a direct dependency would put a storage engine inside a static one-shot binary.
//!
//! The ports are traits rather than a bag of closures because the daemon's obligations should be
//! visible in one place and implementable once, the way `dudes-supervisor`'s `ProcessHost` is.
//!
//! # Where the requirement is written
//!
//! Each method's `Requirement` is written at its [`Dispatcher::register`] call in
//! [`register_all`], and nowhere else. There is no requirement table in this crate: a second copy
//! of the requirements is a second thing to keep true.

pub mod agent;
pub mod goal;
pub mod journal;
pub mod loop_control;
pub mod memory;
pub mod pty;
pub mod team;
pub mod worktree;

use std::sync::Arc;

use dudes_proto::ConditionedMethod;
use dudes_proto::record::Entry;
use dudes_proto::rpc::agent::{
    AttachParams, HireParams, HireResult, LookParams, McpParams, McpResult, RenameParams,
    RetireParams, WhyResult,
};
use dudes_proto::rpc::goal::{GoalSetParams, GoalSetResult};
use dudes_proto::rpc::memory::MemoryAppendParams;
use dudes_proto::rpc::memory::{MemoryReadResult, MemorySearchOptions, MemorySearchResult};
use dudes_proto::rpc::pty::{PtyId, PtyWriteParams};
use dudes_proto::rpc::{AgentRef, AgentText, CurrentMethod, Empty, ErrorCode, Method, RpcError};
use dudes_proto::{PtyCloseOutcome, PtyWriteOutcome};
use dudes_types::{AgentId, Lsn};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::Requirement;
use crate::conditioned::PersistenceOperation;
use crate::dispatch::{Call, Dispatcher, error};
use crate::tail::Subscribers;

/// What the daemon does when an operator asks for something to change.
///
/// **Every method here returns only after the effect is journaled and acknowledged.** Invariant 2
/// is `Persist before Spawn`, and a door that answered `ok` before the record was durable would
/// hand the operator a promise a crash could take back. The transport cannot enforce that — it is
/// the implementor's obligation, stated here because this is where it is relied on.
///
/// `Send + Sync + 'static` because one implementation is shared by every connection thread.
pub trait Control: Send + Sync + 'static {
    /// Hire a managed agent.
    ///
    /// # Errors
    ///
    /// The contract code the refusal maps to — [`ErrorCode::Refused`] for a gate, and so on.
    fn hire(&self, params: HireParams) -> Result<HireResult, RpcError>;

    /// Retire an agent and withdraw its identity.
    ///
    /// The default keeps existing out-of-tree adapters source-compatible while refusing safely;
    /// a control that does not implement retirement must never report success for it.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] until the implementor supplies a durable retirement path.
    fn retire(&self, params: RetireParams) -> Result<(), RpcError> {
        let _ = params;
        Err(error(
            ErrorCode::InternalError,
            "agent.retire is not implemented by this control",
        ))
    }

    /// Durably change a live agent's display name.
    ///
    /// # Errors
    ///
    /// Refuses safely until a control supplies the durable implementation.
    fn rename(&self, params: RenameParams) -> Result<(), RpcError> {
        let _ = params;
        Err(error(
            ErrorCode::InternalError,
            "agent.rename is not implemented by this control",
        ))
    }

    /// Durably change a live agent's role and authored appearance.
    ///
    /// # Errors
    ///
    /// Refuses safely until a control supplies the durable implementation.
    fn look(&self, params: LookParams) -> Result<(), RpcError> {
        let _ = params;
        Err(error(
            ErrorCode::InternalError,
            "agent.look is not implemented by this control",
        ))
    }

    /// Read or durably replace the desired servers for later managed launches.
    ///
    /// # Errors
    /// Refuses safely until a control supplies the durable implementation.
    fn mcp(&self, params: McpParams) -> Result<McpResult, RpcError> {
        let _ = params;
        Err(error(
            ErrorCode::InternalError,
            "agent.mcp is not implemented by this control",
        ))
    }

    /// Every worktree lease that is not removed, with its path.
    ///
    /// # Errors
    ///
    /// Refuses safely until a control supplies the implementation.
    fn worktree_list(&self) -> Result<Vec<dudes_proto::rpc::worktree::WorktreeEntry>, RpcError> {
        Err(error(
            ErrorCode::InternalError,
            "worktree.list is not implemented by this control",
        ))
    }

    /// Journal that the operator removed a released lease's tree.
    ///
    /// # Errors
    ///
    /// Refuses safely until a control supplies the implementation.
    fn worktree_removed(
        &self,
        params: dudes_proto::rpc::worktree::RemovedParams,
    ) -> Result<(), RpcError> {
        let _ = params;
        Err(error(
            ErrorCode::InternalError,
            "worktree.removed is not implemented by this control",
        ))
    }

    /// Give an agent a goal.
    ///
    /// # Errors
    ///
    /// As [`Control::hire`].
    fn set_goal(
        &self,
        params: GoalSetParams,
        hang_up: &crate::server::HangUp,
    ) -> Result<GoalSetResult, RpcError>;
    /// Atomically replace current text and check while preserving remaining allowance.
    fn replace_goal(
        &self,
        _params: dudes_proto::rpc::goal::GoalReplaceParams,
    ) -> Result<dudes_proto::rpc::goal::GoalReplaceResult, RpcError> {
        Err(crate::dispatch::error(
            ErrorCode::MethodNotFound,
            "goal.replace is not implemented",
        ))
    }

    /// Stop issuing turns for an agent.
    ///
    /// # Errors
    ///
    /// As [`Control::hire`].
    fn pause(&self, agent: AgentId) -> Result<(), RpcError>;

    /// Lift a pause.
    ///
    /// # Errors
    ///
    /// As [`Control::hire`].
    fn resume(&self, agent: AgentId) -> Result<(), RpcError>;

    /// Halt an agent; a running turn ends at its next boundary.
    ///
    /// # Errors
    ///
    /// As [`Control::hire`].
    fn halt(&self, agent: AgentId) -> Result<(), RpcError>;

    /// Record that `agent` asked its human `text`, and hold it at gate 3 until the answer.
    ///
    /// **`agent` is the identity the seam resolved from the caller's token**, never a params
    /// field: [`AskParams`](dudes_proto::rpc::agent::AskParams) has no `agent` at all.
    /// An implementor is being told who called, not who to charge it to.
    ///
    /// **The second-question refusal lives here, in the implementor**, and that is the one place
    /// it can live. It reads the projection's open question and must do so on the same thread,
    /// before the append — a gate in the handler above would need a round trip for the projection
    /// and would leave a window between reading it and writing the record.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Refused`] with `RefusalGate::OpenQuestion` when the agent already has an open
    /// question; otherwise as [`Control::hire`].
    fn ask(&self, agent: AgentId, text: String) -> Result<(), RpcError>;
}

/// What the daemon durably records for the two conditioned agent text effects.
///
/// Each operation is exactly one of the router's outcomes: a suppression record, or the
/// identity-bearing view from which the daemon builds the applied domain record. Returning
/// acknowledges fsync. This port never receives raw connection identity, and invariant 8's
/// provenance can only be read through [`PersistenceOperation::applied_effect`].
pub trait ConditionedAgentControl: Send + Sync + 'static {
    /// Persist a suppressed or applied `agent.answer`.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the daemon cannot make the selected record durable.
    fn answer(&self, operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError>;

    /// Persist a suppressed or applied `agent.steer`.
    ///
    /// # Errors
    ///
    /// As [`ConditionedAgentControl::answer`].
    fn steer(&self, operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError>;
}

/// The daemon side of the connection-bound `agent.attach` capability.
pub trait ConditionedAttachControl: Send + Sync + 'static {
    /// Persist the selected outcome and, for Apply, open and return the exact PTY.
    fn attach(&self, operation: PersistenceOperation<'_, AttachParams>) -> Result<PtyId, RpcError>;

    /// Queue cleanup when the connection that owns `pty` goes away. This must not block.
    fn connection_lost(&self, agent: AgentId, pty: PtyId);
}

/// The daemon-owned confirmed-close path for one connection-bound `agent.detach`.
pub trait ConditionedDetachControl: Send + Sync + 'static {
    /// Persist a suppression, or close the router-derived PTY with both audit records durable.
    fn detach(
        &self,
        operation: PersistenceOperation<'_, AgentRef>,
        pty: Option<PtyId>,
    ) -> Result<Option<PtyCloseOutcome>, RpcError>;
}

/// What the daemon durably records for a `pty.write` the router selected.
///
/// The applied arm owes the effect's whole order before it returns, and owes it in that order:
/// append and fsync `PtyWriteRequested`; call the host **exactly once**; append and fsync
/// `PtyWriteCompleted` naming the request's position. The returned outcome is the host's confirmed
/// answer, and the router revokes the connection's ownership on anything but `Complete`.
///
/// Returning `Ok` therefore means "both records are durable", never "the bytes landed" — the
/// second question is what the outcome answers, and the two are deliberately separate so a crash
/// between them leaves an unmatched request rather than a lie in either direction.
pub trait ConditionedPtyControl: Send + Sync + 'static {
    /// Persist a suppressed `pty.write`, or run the applied write's full durable order.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the daemon cannot make the selected record durable.
    fn write(
        &self,
        operation: PersistenceOperation<'_, PtyWriteParams>,
    ) -> Result<PtyWriteOutcome, RpcError>;
}

/// What the daemon durably records for conditioned `memory.append`.
///
/// The returned [`Lsn`] is the acknowledgement for the exact append this operation made durable.
/// The router hands it directly to the result sink, so no shared publication can race the reply.
pub trait ConditionedMemoryControl: Send + Sync + 'static {
    /// Persist an applied or quarantined `memory.append`.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the daemon cannot make the selected record durable.
    fn append(
        &self,
        operation: PersistenceOperation<'_, MemoryAppendParams>,
    ) -> Result<Lsn, RpcError>;
}

/// The loop-owned applied-memory read and current retirement gate.
pub trait MemoryRead: Send + Sync + 'static {
    /// Read a bounded page after `since_lsn`; `caller` is the resolved agent, if any.
    ///
    /// # Errors
    /// Refuses a retired caller before disclosing bytes and reports unknown targets.
    fn read(
        &self,
        agent: AgentId,
        caller: Option<AgentId>,
        since_lsn: Option<Lsn>,
    ) -> Result<MemoryReadResult, RpcError>;
}

/// Register the memory read.
pub fn register_memory_read<M: MemoryRead>(dispatcher: &mut Dispatcher, memory: &Arc<M>) {
    dispatcher.register(
        Method::MemoryRead,
        Requirement::OperatorOrAgent,
        memory::read(Arc::clone(memory)),
    );
}

/// The loop-owned memory search and current retirement gate.
pub trait MemorySearch: Send + Sync + 'static {
    /// Search one resolved target after refusing a retired agent caller.
    ///
    /// # Errors
    /// Refuses retired callers and oversized legacy candidates, and reports unknown targets.
    fn search(
        &self,
        agent: AgentId,
        caller: Option<AgentId>,
        options: MemorySearchOptions,
    ) -> Result<MemorySearchResult, RpcError>;
}

/// Register search for proven agents and operators.
pub fn register_memory_search<M: MemorySearch>(dispatcher: &mut Dispatcher, memory: &Arc<M>) {
    dispatcher.register(
        Method::MemorySearch,
        Requirement::OperatorOrAgent,
        memory::search(Arc::clone(memory)),
    );
}

/// The loop-owned public roster and current retirement gate.
pub trait TeamRoster: Send + Sync + 'static {
    /// Read current colleagues after refusing a retired resolved caller.
    ///
    /// # Errors
    /// Refuses retired callers and reports projection read failures.
    fn roster(
        &self,
        caller: Option<AgentId>,
    ) -> Result<Vec<dudes_proto::rpc::team::RosterEntry>, RpcError>;
}

/// Register the read-only Agent and Operator roster.
pub fn register_team_roster<T: TeamRoster>(dispatcher: &mut Dispatcher, team: &Arc<T>) {
    dispatcher.register(
        Method::TeamRoster,
        Requirement::OperatorOrAgent,
        team::roster(Arc::clone(team)),
    );
}

/// What the daemon can read back out of the journal.
///
/// Read-only, and read through the journal's read-only reader on the daemon's side. Nothing on this
/// surface writes to the journal: a method that changes state emits an effect the daemon persists,
/// which is [`Control`].
///
/// `Send + Sync + 'static` for the same reason as [`Control`].
pub trait JournalRead: Send + Sync + 'static {
    /// Every entry from `from_lsn`, in lsn order. `None` means "nothing yet" — a subscriber that
    /// asked for the tail wants what comes next, not what already happened.
    ///
    /// Called with the subscriber registry locked, so an implementation should read and return
    /// rather than do anything slow it could have done earlier.
    ///
    /// **The return type is a `Vec` on purpose, and it is not an inefficiency to optimise away.**
    /// A journal reader's *iterator* lists the segments once, when it is opened, so an iterator
    /// held open across a rotation yields a rotation-dependent suffix and silently misses whatever
    /// lands in a segment created after it. Measured: an iterator opened over a 40-record journal
    /// while 8 more were appended across two rotations yielded 7 of the 13 records at or after its
    /// starting point. A fresh read, however, does see everything — the same reader re-read the
    /// whole 48 — so the safe shape is "read to a `Vec` here, and let
    /// [`Subscribers::appended`](crate::Subscribers::appended) carry everything after".
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the journal could not be read, or [`ErrorCode::NotFound`]
    /// when `from_lsn` names a position the journal no longer holds.
    fn entries_from(&self, from_lsn: Option<Lsn>) -> Result<Vec<(Lsn, Entry)>, RpcError>;

    /// The last `LoopDecided` for `agent`, with its gate trace.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::NotFound`] when there is no such agent or it has not been decided about yet.
    fn last_decision(&self, agent: AgentId) -> Result<WhyResult, RpcError>;
}

/// Register the durable control methods with their requirements.
///
/// One function rather than separate daemon call sites, so that a method cannot be wired up
/// with a requirement of the daemon's own choosing. Each method's requirement is written
/// at its registration below.
pub fn register_all<C: Control, J: JournalRead>(
    dispatcher: &mut Dispatcher,
    control: &Arc<C>,
    journal: &Arc<J>,
    subscribers: &Arc<Subscribers>,
) {
    dispatcher.register(
        Method::AgentHire,
        Requirement::Operator,
        agent::hire(Arc::clone(control)),
    );
    dispatcher.register(
        Method::AgentRetire,
        Requirement::Operator,
        agent::retire(Arc::clone(control)),
    );
    dispatcher.register(
        Method::AgentRename,
        Requirement::Operator,
        agent::rename(Arc::clone(control)),
    );
    dispatcher.register(
        Method::AgentLook,
        Requirement::Operator,
        agent::look(Arc::clone(control)),
    );
    dispatcher.register(
        Method::AgentMcp,
        Requirement::Operator,
        agent::mcp(Arc::clone(control)),
    );
    dispatcher.register(
        Method::WorktreeList,
        Requirement::Operator,
        worktree::list(Arc::clone(control)),
    );
    dispatcher.register(
        Method::WorktreeRemoved,
        Requirement::Operator,
        worktree::removed(Arc::clone(control)),
    );
    dispatcher.register(
        Method::AgentWhy,
        Requirement::Operator,
        agent::why(Arc::clone(journal)),
    );
    dispatcher.register(
        Method::GoalSet,
        Requirement::Operator,
        goal::set(Arc::clone(control)),
    );
    dispatcher.register(
        Method::GoalReplace,
        Requirement::Operator,
        goal::replace(Arc::clone(control)),
    );
    dispatcher.register(
        Method::LoopPause,
        Requirement::Operator,
        loop_control::pause(Arc::clone(control)),
    );
    dispatcher.register(
        Method::LoopResume,
        Requirement::Operator,
        loop_control::resume(Arc::clone(control)),
    );
    dispatcher.register(
        Method::LoopHalt,
        Requirement::Operator,
        loop_control::halt(Arc::clone(control)),
    );
    dispatcher.register(
        Method::JournalTail,
        Requirement::Operator,
        journal::tail(Arc::clone(journal), Arc::clone(subscribers)),
    );
    // `agent.ask` is `Agent`-only. The **only** non-`Operator` method this function
    // registers, and the first `Requirement::Agent` registration in the tree: `Agent` alone and
    // not `OperatorOrAgent`, because an operator with a question does not need the daemon's help
    // to ask it, and an operator peer able to call this could park any agent in `Wait` without
    // holding that agent's token.
    dispatcher.register_bounded_question(Requirement::Agent, agent::ask(Arc::clone(control)));
}

/// Register the two conditioned agent text effects through the dedicated conditioned route.
///
/// The method and requirement live here rather than in the daemon so neither effect can be
/// accidentally downgraded to an ordinary handler. Invariant 8 is completed by the router's
/// operator-authority and exact-provenance checks before either persistence callback can apply.
pub fn register_agent_effects<C: ConditionedAgentControl>(
    dispatcher: &mut Dispatcher,
    control: &Arc<C>,
) {
    let answer_persist = Arc::clone(control);
    dispatcher.register_conditioned(
        ConditionedMethod::AgentAnswer,
        Requirement::Operator,
        |value, _role| agent::conditioned_text(Method::AgentAnswer, value),
        move |operation| answer_persist.answer(operation),
        |applied, ()| {
            let _ = applied.into_params();
            encode(Method::AgentAnswer, &Empty {})
        },
    );

    let steer_persist = Arc::clone(control);
    dispatcher.register_conditioned(
        ConditionedMethod::AgentSteer,
        Requirement::Operator,
        |value, _role| agent::conditioned_text(Method::AgentSteer, value),
        move |operation| steer_persist.steer(operation),
        |applied, ()| {
            let _ = applied.into_params();
            encode(Method::AgentSteer, &Empty {})
        },
    );
}

/// Register `agent.attach` through the conditioned route and bind its result to one connection.
pub fn register_agent_attach<C: ConditionedAttachControl>(
    dispatcher: &mut Dispatcher,
    control: &Arc<C>,
) {
    let persist = Arc::clone(control);
    let lost = Arc::clone(control);
    dispatcher.register_conditioned_attach(
        Requirement::Operator,
        agent::conditioned_attach,
        move |operation| persist.attach(operation),
        Arc::new(move |agent, pty| lost.connection_lost(agent, pty)),
    );
}

/// Register `agent.detach` on its connection-owning conditioned route.
pub fn register_agent_detach<C: ConditionedDetachControl>(
    dispatcher: &mut Dispatcher,
    control: &Arc<C>,
) {
    let persist = Arc::clone(control);
    dispatcher.register_conditioned_detach(
        Requirement::Operator,
        agent::conditioned_detach,
        move |operation, pty| persist.detach(operation, pty),
    );
}

/// Register `pty.write` through the conditioned route that consumes attachment ownership.
///
/// The method and requirement live here rather than in the daemon for [`register_agent_effects`]'s
/// reason. The requirement is `Operator`; the ownership condition the conditioned router adds on
/// top of it is the router's and cannot be satisfied by any credential, only by holding the
/// binding `agent.attach` recorded on this connection.
pub fn register_pty_write<C: ConditionedPtyControl>(dispatcher: &mut Dispatcher, control: &Arc<C>) {
    let persist = Arc::clone(control);
    dispatcher.register_conditioned_write(
        Requirement::Operator,
        pty::conditioned_write,
        move |operation| persist.write(operation),
    );
}

/// Register `memory.append` through the conditioned route.
pub fn register_memory_effect<C: ConditionedMemoryControl>(
    dispatcher: &mut Dispatcher,
    control: &Arc<C>,
) {
    let persist = Arc::clone(control);
    dispatcher.register_conditioned(
        ConditionedMethod::MemoryAppend,
        Requirement::OperatorOrAgent,
        memory::conditioned_append,
        move |operation| persist.append(operation),
        |applied, lsn| {
            let _ = applied.into_params();
            encode(
                Method::MemoryAppend,
                &dudes_proto::rpc::memory::MemoryAppendResult { lsn },
            )
        },
    );
}

/// Decode a call's params, or refuse the call with [`ErrorCode::InvalidParams`].
///
/// Absent params decode as an empty object rather than as `null`, so a method whose params are all
/// optional can be called with no `params` key at all — which is what `journal.tail` from the tail
/// of the journal looks like on the wire.
///
/// A decode failure is an error *response*: the frame was well formed and the method exists, so the
/// caller is told which of its fields is wrong and keeps its connection.
///
/// # Errors
///
/// [`ErrorCode::InvalidParams`] when the params are not `P`.
pub fn params<P: DeserializeOwned>(call: &Call<'_>) -> Result<P, RpcError> {
    let value = call
        .params
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|err| {
        error(
            ErrorCode::InvalidParams,
            format!("{}: {err}", call.method.name()),
        )
    })
}

/// Refuse a `text` holding U+0000 as invalid params, before any effect.
///
/// `memory.append` and `goal.set` text reaches the agent's next Managed task, and no launch can
/// carry U+0000: a text holding one would leave its agent decided `Continue` with no turn issued.
/// It is refused, never rewritten, and every other scalar is admitted.
///
/// # Errors
///
/// [`ErrorCode::InvalidParams`], with no `data`, naming the method and the byte offset of the
/// first U+0000.
pub(crate) fn refuse_nul(method: Method, text: &str) -> Result<(), RpcError> {
    if let Some(offset) = text.find('\0') {
        return Err(error(
            ErrorCode::InvalidParams,
            format!(
                "{}: text contains U+0000 at byte offset {offset}",
                method.name()
            ),
        ));
    }
    Ok(())
}

/// Encode a contract result, or report the daemon's own failure.
///
/// # Errors
///
/// [`ErrorCode::InternalError`] when the result could not be serialized.
pub fn encode(method: Method, result: &impl Serialize) -> Result<Value, RpcError> {
    encode_current(method.into(), result)
}

/// Encode a result for a current contract method.
///
/// # Errors
///
/// [`ErrorCode::InternalError`] when the result could not be serialized.
pub fn encode_current(method: CurrentMethod, result: &impl Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(result).map_err(|err| {
        error(
            ErrorCode::InternalError,
            format!("could not encode the {} result: {err}", method.name()),
        )
    })
}

#[cfg(test)]
mod tests {
    use dudes_proto::rpc::journal::JournalTailParams;
    use dudes_proto::rpc::{AgentRef, Empty};
    use dudes_types::Lsn;
    use serde_json::json;

    use super::*;
    use crate::auth::PeerRole;
    use crate::outbound::Outbound;

    fn call<'a>(
        method: Method,
        params: Option<&'a Value>,
        role: &'a PeerRole,
        outbound: &'a Arc<Outbound>,
    ) -> Call<'a> {
        Call {
            goal_replacement: None,
            method: method.into(),
            params,
            role,
            outbound,
            hang_up: crate::server::HangUp::never(),
        }
    }

    /// Absent params are an empty object, so a method whose fields are all optional is callable
    /// with no `params` key — and a method whose fields are not is still refused.
    #[test]
    fn absent_params_decode_as_an_empty_object() {
        let role = PeerRole::Unproven;
        let out = Arc::new(Outbound::sink());

        let tail: JournalTailParams =
            params(&call(Method::JournalTail, None, &role, &out)).expect("all optional");
        assert_eq!(tail.from_lsn, None);
        let empty: Empty = params(&call(Method::LoopPause, None, &role, &out)).expect("no fields");
        assert_eq!(empty, Empty {});

        let refused = params::<AgentRef>(&call(Method::LoopPause, None, &role, &out))
            .expect_err("a required field is still required");
        assert_eq!(refused.code, ErrorCode::InvalidParams.code());
    }

    /// A params object of the wrong shape is `-32602`, and the message names the method so an
    /// operator reading a log can tell which call was wrong.
    #[test]
    fn malformed_params_are_invalid_params() {
        let role = PeerRole::Unproven;
        let out = Arc::new(Outbound::sink());
        let wrong = json!({ "agent": 7 });

        let refused = params::<AgentRef>(&call(Method::LoopPause, Some(&wrong), &role, &out))
            .expect_err("an integer is not an AgentId");
        assert_eq!(refused.code, ErrorCode::InvalidParams.code());
        assert!(
            refused.message.contains("loop.pause"),
            "the message names the method: {}",
            refused.message
        );

        // Positive control: the same decode with a well-formed value succeeds, so the code above is
        // about the params and not about a decoder that refuses everything.
        let right = json!({ "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV" });
        let parsed: AgentRef = params(&call(Method::LoopPause, Some(&right), &role, &out))
            .expect("a well-formed AgentRef");
        assert_eq!(parsed.agent.to_string(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    }

    /// `Empty` encodes as `{}` — the contract's "ok", and what six of these nine methods answer.
    #[test]
    fn the_empty_result_encodes_as_an_empty_object() {
        assert_eq!(encode(Method::LoopPause, &Empty {}).unwrap(), json!({}));
        assert_eq!(
            encode(Method::SystemPing, &Lsn(7)).unwrap(),
            json!(7),
            "positive control: encode is not returning {{}} for everything"
        );
    }

    /// An unknown key in a params object is `-32602`, not a silent drop.
    ///
    /// This is the door's half of the contract's `deny_unknown_fields`: `params` decodes with a
    /// plain `from_value`, so whether a misspelled key reaches the operator as an error is
    /// decided entirely by the params type. `sandboxx` is the measured case — accepted and
    /// dropped, which made a setting that did nothing indistinguishable from one that was never
    /// read.
    ///
    /// **The control spells `name`, not `sandbox`, and that is deliberate.** Two refusals now sit
    /// on this path and they are easy to confuse from outside: a *misspelled* key is
    /// `-32602 InvalidParams` here, in the decode, while a *correctly spelled* explicit `sandbox`
    /// is `-32004 Refused` further along, in the handler, because no adapter can carry the
    /// instruction. `params` runs first, so both codes are reachable and each means
    /// what it says. A control keyed on `sandbox` would read as a decoder bug the day it met the
    /// second refusal.
    #[test]
    fn an_unknown_params_key_is_refused_not_dropped() {
        use dudes_proto::rpc::agent::HireParams;

        let role = PeerRole::Unproven;
        let out = Arc::new(Outbound::sink());
        let hire = |name_key: &str, extra: Option<&str>| {
            let mut object = serde_json::Map::new();
            object.insert(name_key.to_owned(), json!("Hector"));
            for (key, value) in [
                ("role", json!("backend")),
                ("department", json!("engineering")),
                ("provider", json!("claude")),
                ("cwd", json!("/repo")),
                ("budget", json!({ "usd_per_day": 5_000_000 })),
            ] {
                object.insert(key.to_owned(), value);
            }
            if let Some(key) = extra {
                object.insert(key.to_owned(), json!(true));
            }
            Value::Object(object)
        };
        let refuses = |sent: &Value, key: &str| {
            let refused = params::<HireParams>(&call(Method::AgentHire, Some(sent), &role, &out))
                .expect_err("an unknown key is refused");
            assert_eq!(refused.code, ErrorCode::InvalidParams.code());
            assert!(
                refused.message.contains("agent.hire")
                    && refused.message.contains(&format!("unknown field `{key}`")),
                "the message names the method and the key the operator got wrong: {}",
                refused.message
            );
        };

        // The measured case: a key that is nobody's field.
        refuses(&hire("name", Some("sandboxx")), "sandboxx");
        // And a real field misspelled, which is the mistake an operator actually makes.
        refuses(&hire("namex", None), "namex");

        // Control: the same object, every key spelled correctly, still decodes — so the two
        // refusals above are about the unknown key and not about a decoder that refuses
        // everything.
        let right = hire("name", None);
        let parsed: HireParams = params(&call(Method::AgentHire, Some(&right), &role, &out))
            .expect("a correctly spelled params object still decodes");
        assert_eq!(parsed.name, "Hector");
        assert_eq!(
            parsed.sandbox, None,
            "an absent optional field is still absent, not defaulted to a value"
        );
    }
}
