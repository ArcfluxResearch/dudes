//! `agent.hire`, `agent.ask`, `agent.answer`, `agent.steer` and `agent.why`.

use std::sync::Arc;

use dudes_proto::ConditionedTarget;
use dudes_proto::rpc::agent::{
    AskParams, AttachParams, HireParams, LookParams, McpParams, RenameParams, RetireParams,
};
use dudes_proto::rpc::{AgentRef, AgentText, Empty, ErrorCode, Method, RpcError};
use dudes_types::Provider;
use serde_json::Value;

use crate::auth::PeerRole;
use crate::conditioned::DecodedEffect;
use crate::dispatch::{Call, error};
use crate::methods::{Control, JournalRead, encode, params};

/// Decode one conditioned agent text effect from the contract shape.
///
/// The target and payload are derived exactly once here, before the central disposition
/// table sees the call. Identity and provenance are deliberately absent: invariant 8 requires
/// those facts to come from the accepted connection, never from params.
///
/// An `agent.answer` whose text holds U+0000 is invalid params. The answer is carried
/// verbatim into the agent's next Managed task, and no launch can carry U+0000, so it could never
/// reach the agent. It stops here, before any disposition, with nothing appended and the question
/// still open. Every other scalar stays admitted, and `agent.steer` is unchanged.
pub(crate) fn conditioned_text(
    method: Method,
    value: Option<&Value>,
) -> Result<DecodedEffect<AgentText>, RpcError> {
    let request: AgentText = serde_json::from_value(
        value
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
    )
    .map_err(|err| {
        error(
            ErrorCode::InvalidParams,
            format!("{}: {err}", method.name()),
        )
    })?;
    if method == Method::AgentAnswer
        && let Some(offset) = request.text.find('\0')
    {
        return Err(error(
            ErrorCode::InvalidParams,
            format!(
                "{}: text contains U+0000 at byte offset {offset}",
                method.name()
            ),
        ));
    }
    let payload = request.text.as_bytes().to_vec();
    let target = ConditionedTarget::Agent {
        agent: request.agent,
    };
    Ok(DecodedEffect::new(request, target, Some(payload)))
}

/// Decode `agent.attach` before the central conditioned disposition is selected.
pub(crate) fn conditioned_attach(
    value: Option<&Value>,
    _role: &PeerRole,
) -> Result<DecodedEffect<AttachParams>, RpcError> {
    let request: AttachParams = serde_json::from_value(
        value
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
    )
    .map_err(|err| error(ErrorCode::InvalidParams, format!("agent.attach: {err}")))?;
    Ok(DecodedEffect::new(
        request,
        ConditionedTarget::Agent {
            agent: request.agent,
        },
        None,
    ))
}

/// Decode `agent.detach`'s existing agent-only params before disposition.
pub(crate) fn conditioned_detach(
    value: Option<&Value>,
    _role: &PeerRole,
) -> Result<DecodedEffect<AgentRef>, RpcError> {
    let request: AgentRef = serde_json::from_value(
        value
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
    )
    .map_err(|err| error(ErrorCode::InvalidParams, format!("agent.detach: {err}")))?;
    Ok(DecodedEffect::new(
        request,
        ConditionedTarget::Agent {
            agent: request.agent,
        },
        None,
    ))
}

/// The ceiling on an `agent.ask` `text`, in **bytes**.
///
/// # Why a whole order of magnitude below the frame cap
///
/// The transport enforces [`MAX_FRAME_BYTES`] on the way *in* and not on the way *out*: an
/// over-cap outbound frame is fatal to its connection, and `journal.tail` backfills, so a question
/// the daemon accepted and journaled can poison every subscriber on every reconnect for ever. So
/// what bounds `text` is not the inbound frame — every envelope the daemon later wraps around the
/// same text is larger than the one it arrived in.
///
/// [`MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
///
/// **The number is not today's largest envelope subtracted from the cap**, and the cap's purpose bars
/// picking it that way: the worst measured over-run was 294 bytes on a *three*-step gate trace,
/// while `Gate::ORDER` has seven gates and each can carry a note, so a cap tuned by subtraction is
/// one a longer trace re-breaks — silently, and durably, because the record is already on the disk.
/// The bound is therefore a ratio with room in it, and the ratio is pinned below. The
/// `a_question_at_the_cap_fits_a_seven_step_loop_decided_wait_frame` test measures the worst byte
/// pattern the handler admits at more than 13 of the 16 reserved cap-lengths, leaving less than
/// 3/13 (23.1%) of that measured size as headroom — under 1.231× total growth — before the
/// derivation comes into question.
///
/// **The value inside the sensible range is a choice**, and it is recorded as one: the range is
/// 8 KiB to 64 KiB and this takes the upper. Preferring the lower means changing this
/// constant, and nothing else moves.
pub const MAX_ASK_TEXT_BYTES: usize = 65_536;

/// The derivation above, checked **by the compiler on every build** rather than by a test.
///
/// A test would only fail where someone ran it; this fails where someone edits either constant, in
/// the crate that owns both. It is the shape a derived constant deserves — the value is not a
/// number that happens to work, it is a number that stands in a stated relation to another one, and
/// this is that relation.
///
/// *Measured control:* replacing [`MAX_ASK_TEXT_BYTES`] with `MAX_FRAME_BYTES / 2` fails the build
/// here, so the assertion is not vacuously true of anything.
const _: () = assert!(
    MAX_ASK_TEXT_BYTES * 16 <= crate::codec::MAX_FRAME_BYTES,
    "the agent.ask cap must leave a whole order of magnitude under the frame cap, \
     because a cap tuned by subtracting today's envelope is one a longer gate trace re-breaks"
);

/// The floor, for the same reason and by the same mechanism.
///
/// The ratio above is satisfied by **zero**, and a cap of zero would refuse every question while
/// every assertion in this file about *refusing* stayed green — the failure a one-sided bound
/// always has. The sensible range is 8 KiB to 64 KiB, so the lower of the two is the floor, and the
/// value above has to stay a size a question can actually be.
const _: () = assert!(
    MAX_ASK_TEXT_BYTES >= 8 * 1024,
    "The agent.ask cap must be 8 KiB to 64 KiB; a cap below its floor refuses questions it should admit"
);

/// `agent.hire`: create a managed agent.
///
/// The params are the contract's [`HireParams`] and the result its `HireResult` — including
/// `sandbox`, which is `Option<bool>` in the contract and stays that way here: "leave it to the
/// provider's default" and "turn it off" are different instructions, and collapsing them into a
/// `bool` would silently pick one.
///
/// **An explicit `sandbox` is refused here.** The private `sandbox_refused` below carries the
/// reason, what the refusal costs, and why it is this handler's job rather than the supervisor's.
///
/// **An explicit zero `max_turns` or `max_budget` is refused here too**, by `zero_limit_refused`,
/// and so is a role holding a control character, which a terminal printing it would obey, and a
/// relative `cwd`, which every later reader would resolve against the daemon's own directory.
pub fn hire<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let mut request: HireParams = params(&call)?;
        request.name = dudes_types::DisplayName::parse(&request.name)
            .map_err(|err| error(ErrorCode::InvalidParams, err.to_string()))?
            .into_string();
        if request.role.chars().any(char::is_control) {
            return Err(error(
                ErrorCode::InvalidParams,
                "role must hold no control character",
            ));
        }
        if request.role.len() > dudes_types::MAX_DISPLAY_ROLE_BYTES {
            return Err(error(
                ErrorCode::InvalidParams,
                format!(
                    "role must contain at most {} UTF-8 bytes",
                    dudes_types::MAX_DISPLAY_ROLE_BYTES
                ),
            ));
        }
        if !request.cwd.is_absolute() {
            return Err(error(
                ErrorCode::InvalidParams,
                format!("cwd `{}` is not an absolute path", request.cwd.display()),
            ));
        }
        if let Some(wanted) = request.sandbox {
            return Err(sandbox_refused(request.provider, wanted));
        }
        if request.max_turns == Some(0) {
            return Err(zero_limit_refused("max_turns"));
        }
        if request.max_budget.is_some_and(|usd| usd.micros() == 0) {
            return Err(zero_limit_refused("max_budget"));
        }
        let result = control.hire(request)?;
        encode(Method::AgentHire, &result)
    }
}

/// The refusal an operator gets for `max_turns: 0` or `max_budget: 0`
///
/// **Zero is not a third meaning, so it does not get one.** Absence and `null` already mean *no
/// cap of ours, the provider's own behaviour only*; a positive value means that number. Zero is
/// left over, and every reading a caller might have of it — unlimited, provider default, no work
/// at all — is a different instruction from the other two. Admitting it would have to pick one
/// silently and record the pick for the agent's whole life, because the run limits give these
/// fields no update method.
///
/// **`InvalidParams`, not `Refused`.** The neighbouring negative, fractional, nonnumeric and
/// out-of-range values are `-32602` from the typed decode a few lines above, and zero is the same
/// kind of mistake by the same caller: a number the field does not accept. Giving it `-32004`
/// instead would tell a client that the daemon's *state or configuration* turned a well-formed
/// request down, which is what that code means and is not what happened here.
///
/// **In front of [`Control::hire`], for the reason `sandbox_refused` is**: `AgentHired` cannot be
/// un-journaled, so a refused hire has to journal nothing, and this handler is the one door both
/// the CLI and a direct JSON-RPC client come through.
fn zero_limit_refused(field: &str) -> RpcError {
    error(
        ErrorCode::InvalidParams,
        format!(
            "`{field}: 0` is not a cap of zero and not unlimited: omit the field for no cap, or              give a positive value"
        ),
    )
}

/// `agent.rename`: admit new display text and durably record it.
pub fn rename<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let mut request: RenameParams = params(&call)?;
        request.name = dudes_types::DisplayName::parse(&request.name)
            .map_err(|err| error(ErrorCode::InvalidParams, err.to_string()))?
            .into_string();
        control.rename(request)?;
        encode(Method::AgentRename, &Empty {})
    }
}

/// `agent.look`: admit bounded display text and durably record the requested appearance.
pub fn look<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let mut request: LookParams = params(&call)?;
        request.role = dudes_types::DisplayRole::parse(&request.role)
            .map_err(|err| error(ErrorCode::InvalidParams, err.to_string()))?
            .into_string();
        control.look(request)?;
        encode(Method::AgentLook, &Empty {})
    }
}

/// `agent.mcp`: read or replace a live agent's desired MCP names.
pub fn mcp<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: McpParams = params(&call)?;
        encode(Method::AgentMcp, &control.mcp(request)?)
    }
}

/// `agent.retire`: durably retire an agent and withdraw its identity.
pub fn retire<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: RetireParams = params(&call)?;
        control.retire(request)?;
        encode(Method::AgentRetire, &Empty {})
    }
}

/// The refusal an operator gets for `sandbox: true` or `sandbox: false`.
///
/// **Both polarities, because both are inert.** `AgentHired.sandbox` reaches `AgentCtx.sandbox`
/// and is read by nothing; no adapter turns a sandbox on, and a trusted
/// repository's own settings can turn one on regardless, so `false` is as false a promise as
/// `true`. Absent and `null` are `None`, mean *the provider's configured default*, and are
/// admitted unchanged.
///
/// **Here rather than in the supervisor** because this handler is `Control::hire`'s only door, so
/// one refusal covers the CLI and a direct JSON-RPC caller alike, and because the command path
/// below has no refusal channel to put it in. It fires before `control.hire`, so a refused hire
/// journals nothing: it never reaches persist.
///
/// **The `data` is `None`, and that is the interim rather than the answer.** [`RefusedData`]'s
/// [`RefusalGate`] has no value that means *"the configuration cannot express this"*,
/// and every value it does have says something else,
/// so emitting one would put a false machine-readable reason on the wire. The refusal leaves
/// the gate to whichever value that work lands and has the message carry the meaning until then,
/// which is why the text below is load-bearing and not decoration.
///
/// [`RefusedData`]: dudes_proto::rpc::RefusedData
/// [`RefusalGate`]: dudes_proto::rpc::RefusalGate
fn sandbox_refused(provider: Provider, wanted: bool) -> RpcError {
    error(
        ErrorCode::Refused,
        format!(
            "no {provider} adapter carries a sandbox on-switch, so `sandbox: {wanted}` would \
             promise containment nothing applies; omit the field to get {provider}'s configured \
             default"
        ),
    )
}

/// `agent.ask`: the calling agent asks its human something.
///
/// **The asker is [`PeerRole::Agent`], never a params field.** [`AskParams`] has no `agent`, so
/// there is nothing for a caller to fill in and nothing for this handler to trust: the identity
/// comes from the token the seam already resolved (invariant 8). That is what
/// makes the row safe to grant `Agent` alone — an agent can park *itself* in `Wait` and no other.
///
/// **The cap fires before [`Control::ask`], so an over-cap question journals nothing.** Same shape
/// as the private `sandbox_refused` above: the refusal is in front of the port, not behind it,
/// because the port's contract is that it returns only once the record is durable — and a durable
/// oversized `Record::HumanQuestion` is exactly what [`MAX_ASK_TEXT_BYTES`] exists to prevent.
///
/// The *other* refusal `agent.ask` needs — a second question while one is open — is not here, and
/// deliberately: it reads `AgentRow::open_question`, which lives in the supervisor's projection.
/// A gate here would need a round trip to fetch it and would leave a window between the read and
/// the append. It sits in the daemon's `Ask::Ask` arm instead, on the single-writer loop thread.
pub fn ask<C: Control>(
    control: Arc<C>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: AskParams = params(&call)?;
        let &PeerRole::Agent { agent } = call.role else {
            return Err(not_an_agent());
        };
        if request.text.len() > MAX_ASK_TEXT_BYTES {
            return Err(question_too_long(request.text.len()));
        }
        control.ask(agent, request.text)?;
        encode(Method::AgentAsk, &Empty {})
    }
}

/// The refusal an over-long question gets, the "refused at the handler, never
/// truncated" half.
///
/// **`data` is `None`, for [`sandbox_refused`]'s reason and not for a different one.**
/// [`RefusalGate`] has no value meaning *"the payload is too large"*; `OpenQuestion` means the
/// slot is taken, which is a different refusal on this same method, and emitting it here would put
/// a false machine-readable reason on the wire. So the message carries the meaning, and it names
/// both numbers: an agent that has to shorten a question needs to know by how much.
///
/// [`RefusalGate`]: dudes_proto::rpc::RefusalGate
fn question_too_long(len: usize) -> RpcError {
    error(
        ErrorCode::Refused,
        format!(
            "the question is {len} bytes and the ceiling is {MAX_ASK_TEXT_BYTES}; it is refused \
             rather than truncated, because a truncated question is one whose meaning the daemon \
             changed and an append cannot be un-journaled"
        ),
    )
}

/// The refusal for a call this handler cannot attribute.
///
/// **Unreachable through the dispatcher**, which checks `Requirement::Agent` at step 5 before any
/// handler runs — so this is the daemon reporting its own registration wrong, not a caller being
/// told something. [`ErrorCode::InternalError`] rather than [`ErrorCode::Unauthorized`] says which
/// of the two it is, and the message says nothing about what the daemon resolved the peer to be:
/// a message that varied with the role would be an oracle for probing the seam.
fn not_an_agent() -> RpcError {
    error(
        ErrorCode::InternalError,
        "agent.ask reached its handler without an agent identity, which means it is registered \
         with something other than the Agent requirement",
    )
}

/// `agent.why`: the last `LoopDecided` for an agent, with its gate trace.
///
/// A read, so it goes to [`JournalRead`] rather than [`Control`]: `dudes why` must be answerable
/// while the loop is paused, halted or busy, and a read that went through the control path would
/// be a read that could be refused by a gate.
pub fn why<J: JournalRead>(
    journal: Arc<J>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: AgentRef = params(&call)?;
        let result = journal.last_decision(request.agent)?;
        encode(Method::AgentWhy, &result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dudes_proto::record::{Entry, Record};
    use dudes_proto::rpc::NotificationKind;
    use dudes_proto::rpc::agent::HireResult;
    use dudes_proto::rpc::goal::{GoalSetParams, GoalSetResult};
    use dudes_proto::rpc::hook::HOOK_TOKEN_LEN;
    use dudes_proto::rpc::journal::JournalAppendedParams;
    use dudes_proto::{Gate, GateStep, LoopDecision};
    use dudes_types::{AgentId, Lsn, Timestamp};

    use super::*;
    use crate::codec::MAX_FRAME_BYTES;
    use crate::outbound::Outbound;
    use crate::tail::Subscribers;

    fn agent_id() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a well-formed ULID")
    }

    fn token() -> String {
        "a".repeat(HOOK_TOKEN_LEN)
    }

    /// A [`Control`] whose only job is to say whether `ask` was reached, and as what.
    ///
    /// Every other method is `unreachable!`: this fixture is driven at one door, and a test that
    /// wandered into another should say so rather than get a canned answer.
    #[derive(Debug, Default)]
    struct Reached {
        asks: Mutex<Vec<(AgentId, usize)>>,
    }

    impl Reached {
        fn asks(&self) -> Vec<(AgentId, usize)> {
            self.asks.lock().expect("the fixture's lock").clone()
        }
    }

    impl Control for Reached {
        fn hire(&self, _params: HireParams) -> Result<HireResult, RpcError> {
            unreachable!("this fixture is only driven at agent.ask")
        }

        fn set_goal(
            &self,
            _params: GoalSetParams,
            _hang_up: &crate::server::HangUp,
        ) -> Result<GoalSetResult, RpcError> {
            unreachable!("this fixture is only driven at agent.ask")
        }

        fn pause(&self, _agent: AgentId) -> Result<(), RpcError> {
            unreachable!("this fixture is only driven at agent.ask")
        }

        fn resume(&self, _agent: AgentId) -> Result<(), RpcError> {
            unreachable!("this fixture is only driven at agent.ask")
        }

        fn halt(&self, _agent: AgentId) -> Result<(), RpcError> {
            unreachable!("this fixture is only driven at agent.ask")
        }

        fn ask(&self, agent: AgentId, text: String) -> Result<(), RpcError> {
            self.asks
                .lock()
                .expect("the fixture's lock")
                .push((agent, text.len()));
            Ok(())
        }
    }

    /// A writer whose bytes a test can read back, shared with the [`Outbound`] built over it.
    ///
    /// Deliberately raw bytes rather than parsed frames: what this file measures is the length
    /// `write_frame` compared against the cap, and that length includes the terminator
    /// (`codec.rs`, `line.push(b'\n')` before the check).
    #[derive(Clone, Debug, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("the sink's lock").clone()
        }
    }

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the sink's lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A `LoopDecided{Wait}` carrying `question`, whose gate trace is **every** gate in
    /// [`Gate::ORDER`], each carrying an ASCII note of `note_len` bytes.
    ///
    /// # This entry is synthetic in two ways, and neither changes its size
    ///
    /// **Its trace is longer than `decide()` emits.** `Gate::Human` is third and `decide()` returns
    /// at the first gate that fires (`dudes-policy/src/loop.rs:26-27`), so a real `Wait` carries
    /// three steps — exactly the trace the cap measurement used, and exactly why that measurement is
    /// useless for choosing a cap.
    ///
    /// **Its `fired` is on `Gate::Human`, which is step 3 of 7, and the tree says the last step
    /// fires** — `dudes-proto/src/record.rs:238` ("in order; the last one fired"),
    /// `dudes-policy/src/input.rs:139` ("exactly the last one fired"), and
    /// `dudes-policy/src/proptest.rs:377-378`, which asserts `fired.last() == Some(&true)` of every
    /// decision `decide()` produces. So no producer can emit this entry. `fired` is a `bool`, so
    /// the frame is the same length whichever step carries it, and what is being measured is a
    /// length.
    ///
    /// **What licenses measuring a shape no producer emits** is the cap's purpose: it bounds what the daemon can
    /// compose around an admitted `text`, not what one gate order reaches today. Seven is the
    /// producer's bound, not the type's — `gate_trace` is a `Vec<GateStep>` and admits any length;
    /// it is `decide()`'s loop over [`Gate::ORDER`] that stops at seven. That is why the test
    /// ratchets `Gate::ORDER.len()` explicitly rather than trusting the `Vec`.
    fn wait_entry(question: String, note_len: usize) -> Entry {
        Entry {
            at: Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::LoopDecided {
                agent: agent_id(),
                // A blake3 digest is 64 lowercase hex characters; the value is irrelevant here and
                // the length is not.
                input_digest: "0".repeat(64),
                decision: LoopDecision::Wait { question },
                // ASCII, and that is the fixture matching the daemon rather than a convenience:
                // a note is composed by `decide()` out of constants, enum names, ids and numbers,
                // never out of anything an agent supplied. See the test's own docstring.
                gate_trace: Gate::ORDER
                    .into_iter()
                    .map(|gate| GateStep {
                        gate,
                        fired: gate == Gate::Human,
                        note: Some("n".repeat(note_len)),
                    })
                    .collect(),
            },
        }
    }

    /// The bytes one `journal.appended` notification put on a subscriber's connection.
    ///
    /// Composed by the **real** fan-out — [`Subscribers::appended`] → `tail::send` → [`Outbound`]
    /// → `write_frame` — rather than by building a `Notification` here, which `tail::send`'s own
    /// documentation calls a second spelling of a wire shape `dudes-proto` owns. Going through it
    /// means a field later added to [`JournalAppendedParams`] is re-measured instead of missed.
    ///
    /// Empty when the frame was refused: `write_frame` writes nothing at all on
    /// `FrameError::TooLarge`, which is what makes the emptiness a usable control.
    fn appended_bytes(entry: &Entry) -> Vec<u8> {
        let sink = Sink::default();
        let out = Arc::new(Outbound::new(sink.clone()));
        let subscribers = Subscribers::new();
        subscribers
            .subscribe(&out, || Ok(Vec::new()))
            .expect("an empty backfill subscribes");
        subscribers.appended(Lsn(1), entry);
        sink.bytes()
    }

    /// Drive the real handler as `agent` with a `text` of `len` bytes, and hand back what it
    /// answered together with the port it did or did not reach.
    fn ask_of(agent: AgentId, len: usize) -> (Result<Value, RpcError>, Arc<Reached>) {
        let control = Arc::new(Reached::default());
        let handler = ask(Arc::clone(&control));
        let role = PeerRole::Agent { agent };
        let outbound = Arc::new(Outbound::sink());
        let sent = serde_json::json!({ "token": token(), "text": "q".repeat(len) });
        let answer = handler(Call {
            goal_replacement: None,
            method: Method::AgentAsk.into(),
            params: Some(&sent),
            role: &role,
            outbound: &outbound,
            hang_up: crate::server::HangUp::never(),
        });
        (answer, control)
    }

    /// An over-cap question is refused **before** [`Control::ask`], so it journals nothing — and
    /// the same call one byte shorter is admitted and does reach the port.
    ///
    /// **The reachability assertion is the load-bearing half.** A handler that refused *after*
    /// calling the port would answer `-32004` identically, having already written a durable
    /// `Record::HumanQuestion` that no `journal.tail` subscriber can ever be sent — which is the
    /// failure [`MAX_ASK_TEXT_BYTES`] exists to prevent. The error code cannot tell
    /// those two handlers apart; `asks()` can.
    ///
    /// **The control is at exactly the cap, not far below it**, so the boundary discriminates on
    /// length rather than on shape: a handler that refused every `agent.ask`, or a fixture that
    /// recorded nothing, fails the second half.
    #[test]
    fn an_over_cap_question_is_refused_before_the_port_and_the_cap_itself_is_admitted() {
        let (refused, control) = ask_of(agent_id(), MAX_ASK_TEXT_BYTES + 1);
        let err = refused.expect_err("one byte over the cap is refused");
        assert_eq!(err.code, ErrorCode::Refused.code());
        assert!(
            control.asks().is_empty(),
            "an over-cap question must not reach Control::ask, or the record is durable before \
             anything refuses it"
        );

        let (admitted, control) = ask_of(agent_id(), MAX_ASK_TEXT_BYTES);
        assert!(
            admitted.is_ok(),
            "control: exactly at the cap is admitted, so the refusal above is about the length \
             and not about the shape of the call: {admitted:?}"
        );
        assert_eq!(
            control.asks(),
            vec![(agent_id(), MAX_ASK_TEXT_BYTES)],
            "and the admitted call reached the port with the whole text, untruncated"
        );
    }

    /// The other half of the cap, and the half a refusal test cannot reach: a question the handler
    /// **admits** must still fit every frame the daemon later composes around it.
    ///
    /// # What is actually asserted
    ///
    /// A `journal.appended(LoopDecided{Wait})` carrying a question at exactly
    /// [`MAX_ASK_TEXT_BYTES`], with a **seven**-step gate trace — every gate in [`Gate::ORDER`],
    /// each carrying an ASCII note as long as the question — measured twice, over the two byte
    /// patterns that bracket what the handler admits.
    ///
    /// **The note length is the derivation, not a number that happened to fit.** The frame carries
    /// `1 + 7 = 8` texts at the cap, and the compile-time assertion above this module reserves
    /// `16` cap-lengths under [`MAX_FRAME_BYTES`]. Nothing here subtracts today's envelope from
    /// the cap, which is the move the cap's purpose forbids.
    ///
    /// # Eight cap-lengths is the ASCII figure; the real one is thirteen
    ///
    /// The cap at `:145` counts **bytes** and validates no content, so an admitted question can be
    /// any valid UTF-8 — and JSON escaping expands it. `U+0001` is one byte in and six out
    /// (`\u0001`), the worst factor there is. Measured, same composition, question varied:
    ///
    /// | question at the cap | frame | of the cap | cap-lengths |
    /// |---|---|---|---|
    /// | `q` | 524,867 | 50.1% | 8 of 16 |
    /// | `U+0001` | 852,547 | 81.3% | **13 of 16** |
    ///
    /// So the accounting to reason with is **13 of 16**, not 8, and the headroom is 196 KiB rather
    /// than 512 KiB. The derivation still holds — and it holds for a reason worth stating, because
    /// it is a fact about the daemon and not about arithmetic: **only one of the eight texts is
    /// agent-supplied.** Every note is composed by `decide()` from constants, enum names, ids and
    /// numbers — `dudes-policy/src/loop.rs:189` puts `card.id` in the note and `card.title` in the
    /// *task* — so seven of the eight cannot escape-expand at all. **If a note ever echoes free
    /// text, this derivation is void and the cap has to be re-derived**, which is what the second
    /// arm below is here to make fail loudly rather than quietly.
    ///
    /// # Why a seven-step trace when a real `Wait` has three
    ///
    /// [`wait_entry`] carries the reason: gate 3 fires, `decide()` returns there, and the measured
    /// over-run of 294 bytes was taken on that three-step trace — which is precisely why the cap is refused
    /// to let the cap be tuned to it. A cap that clears today's shortest trace clears nothing.
    ///
    /// # Which assertion is load-bearing, because it is not the one that reads that way
    ///
    /// [`appended_bytes`] returns either a line `write_frame` has **already** compared against the
    /// cap or an empty vec, and `0 <= MAX_FRAME_BYTES` — so the `<= MAX_FRAME_BYTES` assertions
    /// below **cannot fail** while `codec.rs:105-108` stands. They are the arm that catches that
    /// enforcement being removed, and nothing else.
    ///
    /// What actually carries the "under `MAX_FRAME_BYTES`" claim is the **lower** bound: an
    /// envelope that outgrew the cap makes `write_frame` refuse, the sink stays empty, and the
    /// lower bound reports `0 bytes`. A maintainer meeting that message should look for an
    /// envelope that grew, not for a broken sink.
    ///
    /// And the guard on the ratio itself is neither: it is the `const` assertion at `:48-52`,
    /// which the compiler checks. *Control:* raising [`MAX_ASK_TEXT_BYTES`] to `131_072` is
    /// `error[E0080]` at `agent.rs:48:15`, so that mutation never reaches a test at all.
    ///
    /// # Which arms are coverage and which are anti-vacuity
    ///
    /// Stated because a list of red mutants reads as coverage and mostly is not. Mutating this
    /// fixture — the trace's length, the notes, the fan-out call, the question's length — proves
    /// the assertions read their inputs. It proves nothing about shipped code. Of the mutations
    /// that touch shipped code:
    ///
    /// - **Raising the cap with the ratio relaxed to admit it is caught here and nowhere else** —
    ///   the whole workspace stays green apart from this test.
    /// - **A note that carried agent text** is the second. *Measured:* filling the notes with
    ///   `U+0001` too reddens this test at the lower bound with `0 bytes` — eight escape-expanding
    ///   texts at the cap is 3 MiB and `write_frame` refuses the frame. The first arm is what
    ///   fires, since both arms share the fixture's notes; the `U+0001` arm's own job is to hold
    ///   the documented margin at 13 of 16 rather than at the ASCII 8, so nobody re-derives the
    ///   cap from the wrong figure.
    /// - Deleting `write_frame`'s cap check is caught by five tests that predate this one
    ///   (`codec.rs:237`, `:268`; `server.rs:1187`; `tail.rs:421`, `:485`), so the negative control
    ///   below is a check on this fixture's sink, not new coverage.
    /// - Truncating [`Gate::ORDER`] never reaches a build: `error[E0277]` at `decision.rs:120`,
    ///   from `gate_order_is_fixed`. `Gate::ORDER.len()` below is a ratchet on this fixture.
    ///
    /// **Nothing here covers the "refused rather than truncated" rule.** This test never calls
    /// the handler. That rule is enforced in the handler and covered by the refusal test above and by
    /// `bins/duded/tests/ask.rs`.
    ///
    /// # Controls
    ///
    /// - The **lower** bound. A frame that fits proves nothing if it is not the frame that was
    ///   composed, and an empty sink, a dropped `note` and a shortened trace all satisfy an upper
    ///   bound on their own. `> 8 * MAX_ASK_TEXT_BYTES` is what refuses them.
    /// - The **decode**. A byte count cannot tell a `LoopDecided{Wait}` from anything else of the
    ///   same size, and it cannot see a truncation smaller than its own slack — so the frame is
    ///   read back and its shape, its step count and its question length asserted. The last of
    ///   those is the *never truncated* rule, observed on the wire rather than at the handler.
    /// - The **negative** control, in the same style: lengthen the question alone and the identical
    ///   composition delivers zero bytes. Without it, a fan-out that had silently stopped writing
    ///   would pass every assertion above by way of a sink nobody proved could fill.
    #[test]
    fn a_question_at_the_cap_fits_a_seven_step_loop_decided_wait_frame() {
        assert_eq!(
            Gate::ORDER.len(),
            7,
            "the trace this test bounds is the seven-gate one, not the three-step \
             trace the cap must not be tuned to"
        );

        let frame = appended_bytes(&wait_entry(
            "q".repeat(MAX_ASK_TEXT_BYTES),
            MAX_ASK_TEXT_BYTES,
        ));

        assert!(
            frame.len() <= MAX_FRAME_BYTES,
            "inert while codec.rs:105-108 stands — this fires only if write_frame stops enforcing \
             the cap: the notification is {} bytes against a {MAX_FRAME_BYTES}-byte cap",
            frame.len()
        );
        assert!(
            frame.len() > 8 * MAX_ASK_TEXT_BYTES,
            "the load-bearing bound: the frame that fitted is the one that was composed — an ASCII \
             question and seven notes, each at the cap. `0 bytes` here means an envelope outgrew \
             the frame cap and write_frame refused it, not that the sink broke: {} bytes",
            frame.len()
        );
        let line = frame
            .strip_suffix(b"\n")
            .expect("a frame is a line, and its terminator counts against the cap");
        assert!(
            !line.contains(&b'\n'),
            "one append, one frame: the bytes measured above are that frame and not several"
        );

        let value: Value = serde_json::from_slice(line).expect("the frame is one JSON object");
        assert_eq!(
            value["method"],
            Value::from(NotificationKind::JournalAppended.name())
        );
        let params: JournalAppendedParams =
            serde_json::from_value(value["params"].clone()).expect("the contract's params");
        let Record::LoopDecided {
            decision,
            gate_trace,
            ..
        } = params.entry.record
        else {
            panic!("the fixture composes a LoopDecided")
        };
        assert_eq!(
            gate_trace.len(),
            Gate::ORDER.len(),
            "the trace that fitted is the seven-step one, not a shorter one"
        );
        let LoopDecision::Wait { question } = decision else {
            panic!("the fixture composes a Wait")
        };
        assert_eq!(
            question.len(),
            MAX_ASK_TEXT_BYTES,
            "and the question arrived at the cap, whole: it is refused rather than truncated"
        );

        // The second arm: the same composition over the worst byte pattern the handler admits.
        // `:145` caps bytes and validates no content, so `U+0001` — one byte in, six out as
        // `\u0001` — is as far as an admitted question can expand. This keeps the
        // derivation's stated margin the real one: 13 of 16 cap-lengths, not the ASCII 8.
        let escaped = appended_bytes(&wait_entry(
            "\u{1}".repeat(MAX_ASK_TEXT_BYTES),
            MAX_ASK_TEXT_BYTES,
        ));

        assert!(
            escaped.len() > 13 * MAX_ASK_TEXT_BYTES,
            "the expansion is real and is being measured: six bytes out per byte in on the one \
             agent-supplied text, plus seven ASCII notes, is thirteen cap-lengths — {} bytes",
            escaped.len()
        );
        assert!(
            escaped.len() <= MAX_FRAME_BYTES,
            "and thirteen of the sixteen cap-lengths the ratio reserves still fit one frame, \
             which is the derivation's real margin rather than the ASCII one: {} bytes",
            escaped.len()
        );
        assert!(
            escaped.len() > frame.len(),
            "control: the two arms differ only in the question's bytes, so an escape-expanding \
             question must compose a strictly larger frame than an ASCII one — {} vs {}",
            escaped.len(),
            frame.len()
        );

        let over = appended_bytes(&wait_entry("q".repeat(MAX_FRAME_BYTES), MAX_ASK_TEXT_BYTES));
        assert!(
            over.is_empty(),
            "control on this fixture's sink — the five tests at codec.rs:237, :268, server.rs:1187 \
             and tail.rs:421, :485 are what cover the refusal itself. Lengthening the question \
             alone stops the frame being deliverable, so the fits above are about length and not \
             about a sink that never fills — {} bytes",
            over.len()
        );
    }

    /// The asker is the resolved peer, and there is no params field the handler could read
    /// instead.
    ///
    /// The control is a **second** agent driven with byte-identical params: without it, a handler
    /// that hard-coded an identity — or read one out of a params field that happened to agree —
    /// would pass.
    #[test]
    fn the_asker_is_the_resolved_peer_and_not_anything_in_the_params() {
        let other = AgentId::parse("01BX5ZZKBKACTAV9WEVGEMMVRZ").expect("a well-formed ULID");
        assert_ne!(agent_id(), other, "control: the two ids are different");

        let seen: Vec<AgentId> = [agent_id(), other]
            .into_iter()
            .map(|agent| {
                let (answer, control) = ask_of(agent, 8);
                answer.expect("admitted");
                let asks = control.asks();
                assert_eq!(asks.len(), 1, "one call, one ask");
                asks[0].0
            })
            .collect();

        assert_eq!(
            seen,
            vec![agent_id(), other],
            "identical params reached the port as two different agents, so the asker comes from \
             the resolved role"
        );
    }
}
