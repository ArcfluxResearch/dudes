//! The dispatch path: envelope in, response out, through exactly one authorisation call.
//!
//! The order is fixed and load-bearing:
//!
//! 1. the envelope is well formed (`jsonrpc` is `"2.0"`),
//! 2. what the call presents is extracted,
//! 3. **the seam runs**,
//! 4. only then is the method looked up,
//! 5. **the method's own requirement is checked against the role the seam resolved**,
//! 6. only then does a handler see the call.
//!
//! Step 3 before step 4 is the security-relevant part: a peer that may call nothing is told it may
//! call nothing, and never which method names exist. It also means the seam is asked about names
//! that are not methods at all, which is why [`Authorizer::authorize`] takes a `&str`.
//!
//! Actual connections whose transport cannot report a peer pid have an admission check before
//! this order. It peeks only at the registered requirement and refuses `Agent` and `OperatorOrAgent`
//! grants before invoking the seam. `AnyRole` retains the liveness grant of `system.ping`. Reported-pid
//! calls retain the seam-before-lookup order below. This check selects no classification row or column.
//!
//! # Two questions, two places
//!
//! Step 3 and step 5 are not the same check run twice. The seam answers *what this presentation
//! resolves to* — the peer-classification table — and knows nothing about any method's requirement. Step 5 answers
//! *which roles this method admits* — the method's `Requirement` — and knows nothing about the peer. So
//! the requirement is declared where the method is defined, at its [`Dispatcher::register`] call,
//! citing that method's row; there is no requirement table in this crate for the rows to drift
//! from, and `register` takes the requirement by value so a handler cannot be added without one.
//!
//! Step 5 sits *after* the lookup, which means a peer the seam admitted can still tell a method it
//! may not call (`-32001`) from a method that does not exist (`-32601`). That ordering is
//! deliberate, for step 3's sake — the peer this discloses to is one the seam let
//! in.
//!
//! **Who that is has changed, and the residual is named rather than left implied.** While the only
//! seam was [`PingOnly`](crate::PingOnly), no peer was ever admitted for any name but
//! `system.ping`, so nothing was disclosed to anybody. Under peer classification a peer that
//! presents nothing resolves to `Unproven` — a role, not a closed door — and reaches the lookup for
//! *every* name. So such a peer can now enumerate which methods this build registers, by telling
//! `-32001` from `-32601`.
//!
//! **That subset is not the contract's method list and does not follow from it.**
//! [`methods::register_all`](crate::methods::register_all) registers nine names; the contract's
//! complete method and notification lists are generated into `schemas/rpc.schema.json`. When
//! `agent.ask` landed the public method list grew without changing the registered subset. That
//! subset is therefore strictly smaller than the public list and not derivable from it, so "it is
//! already public" is the wrong reason to accept this.
//!
//! The right reason is stronger. `system.ping` carries [`Requirement::AnyRole`] and answers *any*
//! peer with this build's version — a disclosure that is conceded on purpose, so
//! that a daemon stays probeable without a credential. For an open-source build a version string
//! plus public source **is** the registered subset: the enumeration tells an unproven peer what
//! `git checkout $version` and a grep already tell it. Accepting the residual was right; that is
//! what it rests on, and the reason would have to be re-made for a build whose source is not
//! published.
//!
//! Closing it costs more than it buys either way: it means the seam knowing which methods exist,
//! and a seam that reads the method table is a second copy of the peer classification.
//!
//! The contract version field `v` is not checked here. Framing and version are separate gates and
//! this crate ships only the first.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use dudes_proto::rpc::{
    CurrentMethod, ErrorCode, JSONRPC, Method, RefusalGate, RefusedData, Request, RequestId,
    Response, RpcError,
};
use serde_json::Value;

use crate::auth::{Authorizer, PeerProvenance, PeerRole, Presented, Refusal, Requirement};
use crate::conditioned::{
    AppliedEffect, ConditionedConnectionContext, ConditionedRoute, DecodedEffect,
    PersistenceOperation, PersistenceReceipt, R5Method,
};
use crate::outbound::Outbound;

/// One call, as a handler sees it.
///
/// The role arrives resolved. A handler never re-derives identity and never decides for itself
/// whether it may run — that decision has already been taken, once, by the seam.
///
/// **There is no peer here, and no provenance either.** Both stop at
/// [`Authorizer::authorize`]: a `provenance` field beside [`Call::role`]
/// would be a second classification table inside handlers and a copy of the method requirements. The seven conditioned
/// effects journal resolved provenance through their dedicated router; `agent.ask` is the eighth
/// conditioned row and stays on its separate bounded-question route. Neither is a field on this type.
#[derive(Debug)]
pub struct Call<'a> {
    /// The registered contract method for this call.
    pub method: CurrentMethod,
    /// The params object as it arrived, unvalidated: each handler owns its own schema.
    pub params: Option<&'a Value>,
    /// Strictly decoded original-frame params for `goal.replace`.
    pub goal_replacement: Option<&'a dudes_proto::rpc::goal::GoalReplaceParams>,
    /// What the seam resolved this call to.
    pub role: &'a PeerRole,
    /// The write half of the connection this call arrived on.
    ///
    /// A handler that only answers never touches it — its result is written for it. It is here for
    /// the handlers that *subscribe*: a subscription outlives the call that started it, so it needs
    /// somewhere to send to that is not the return value.
    pub outbound: &'a Arc<Outbound>,
    /// Whether the peer this call arrived from has gone. A handler that waits before its effect
    /// asks it, so a peer that hung up gets no effect.
    pub hang_up: crate::server::HangUp,
}

/// A method handler: a call in, a result value or a JSON-RPC error out.
pub type Handler = Box<dyn Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync>;

/// A handler that also receives the caller's reported process id, for an audit log line only.
///
/// The pid is never an identity (see [`crate::auth::PeerPid`]): the role on [`Call::role`] has
/// already decided whether the call may run.
type AuditedHandler = Box<dyn Fn(Call<'_>, Option<u32>) -> Result<Value, RpcError> + Send + Sync>;

/// A registered method: what it requires, and what runs when it is allowed to.
struct Registered {
    requires: Requirement,
    route: Route,
    after_response: Option<AfterResponse>,
}

enum Route {
    Ordinary(Handler),
    Audited(AuditedHandler),
    BoundedQuestion(Handler),
    Conditioned(ConditionedRoute),
}

type AfterResponse = Box<dyn Fn() + Send + Sync>;

/// One answered frame and work that becomes eligible only after that answer was attempted.
pub(crate) struct DispatchOutcome<'a> {
    response: Response,
    after_response: Option<&'a (dyn Fn() + Send + Sync)>,
}

impl DispatchOutcome<'_> {
    fn response(response: Response) -> Self {
        Self {
            response,
            after_response: None,
        }
    }

    pub(crate) const fn response_ref(&self) -> &Response {
        &self.response
    }

    pub(crate) fn into_response(self) -> Response {
        self.response
    }

    pub(crate) fn run_after_response(self) {
        if let Some(action) = self.after_response {
            action();
        }
    }
}

/// The method table plus the seam every call passes through.
pub struct Dispatcher {
    methods: BTreeMap<CurrentMethod, Registered>,
    authorizer: Box<dyn Authorizer>,
}

impl fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatcher")
            .field(
                "methods",
                &self.methods.keys().map(|m| m.name()).collect::<Vec<_>>(),
            )
            .field("authorizer", &self.authorizer)
            .finish()
    }
}

impl Dispatcher {
    /// An empty table behind `authorizer`.
    #[must_use]
    pub fn new(authorizer: impl Authorizer + 'static) -> Self {
        Self {
            methods: BTreeMap::new(),
            authorizer: Box::new(authorizer),
        }
    }

    /// Register `handler` for `method`, admitting only the roles `requires` names, and replacing
    /// any handler already there.
    ///
    /// The key is [`Method`] rather than a string, so a handler cannot be registered for a name the
    /// contract does not define — a method that no client could ever reach is a bug that a string
    /// key would keep quiet about.
    ///
    /// `requires` is taken by value rather than defaulted for the same reason: the call site is
    /// where the method's requirement belongs, and a requirement that could be omitted
    /// is a requirement a new handler can forget. Write the requirement there; that is where
    /// the requirements live.
    pub fn register(
        &mut self,
        method: Method,
        requires: Requirement,
        handler: impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) -> &mut Self {
        assert!(
            R5Method::from_method(method).is_none(),
            "{} requires its dedicated conditioned registration route",
            method.name()
        );
        self.methods.insert(
            method.into(),
            Registered {
                requires,
                route: Route::Ordinary(Box::new(handler)),
                after_response: None,
            },
        );
        self
    }

    /// Register an ordinary handler with work that runs after its successful result is answered.
    ///
    /// The action is eligible only when `handler` returns `Ok`. The connection loop attempts the
    /// response write and flush first, then runs the action exactly once even when that attempt
    /// fails. Parse, authorisation, requirement and handler failures do not run it.
    pub fn register_after_response(
        &mut self,
        method: Method,
        requires: Requirement,
        handler: impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static,
        after_response: impl Fn() + Send + Sync + 'static,
    ) -> &mut Self {
        assert!(
            R5Method::from_method(method).is_none(),
            "{} requires its dedicated conditioned registration route",
            method.name()
        );
        self.methods.insert(
            method.into(),
            Registered {
                requires,
                route: Route::Ordinary(Box::new(handler)),
                after_response: Some(Box::new(after_response)),
            },
        );
        self
    }

    /// Register an ordinary handler that names its caller's reported pid in its own audit log.
    pub fn register_audited(
        &mut self,
        method: Method,
        requires: Requirement,
        handler: impl Fn(Call<'_>, Option<u32>) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) -> &mut Self {
        self.register_current_audited(method.into(), requires, handler)
    }

    /// Register a current ordinary method with its caller's reported pid for auditing.
    ///
    /// # Panics
    ///
    /// A conditioned method requires its dedicated registration route.
    pub fn register_current_audited(
        &mut self,
        method: CurrentMethod,
        requires: Requirement,
        handler: impl Fn(Call<'_>, Option<u32>) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) -> &mut Self {
        assert!(
            method.legacy().and_then(R5Method::from_method).is_none(),
            "{} requires its dedicated conditioned registration route",
            method.name()
        );
        self.methods.insert(
            method,
            Registered {
                requires,
                route: Route::Audited(Box::new(handler)),
                after_response: None,
            },
        );
        self
    }

    /// Register the `agent.ask` bounded-question method on its disjoint conditioned route.
    ///
    /// There is deliberately no `method` argument: this route can register only `agent.ask`, and
    /// ordinary registration rejects that row together with the seven conditioned effects.
    pub fn register_bounded_question(
        &mut self,
        requires: Requirement,
        handler: impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) -> &mut Self {
        self.methods.insert(
            Method::AgentAsk.into(),
            Registered {
                requires,
                route: Route::BoundedQuestion(Box::new(handler)),
                after_response: None,
            },
        );
        self
    }

    /// Register one of the seven conditioned effects on the central conditioned route.
    ///
    /// `persist` must return only after its record is durable. It receives the disposition record
    /// or the sole identity-bearing applied view; the effect sink receives neither provenance nor
    /// actor. Only an acknowledged applied persistence operation reaches `sink`, and then only
    /// through a private-field, non-`Clone` [`AppliedEffect`].
    pub fn register_conditioned<M, R, D, P, S>(
        &mut self,
        method: dudes_proto::ConditionedMethod,
        requires: Requirement,
        decoder: D,
        persist: P,
        sink: S,
    ) -> &mut Self
    where
        M: 'static,
        R: PersistenceReceipt + 'static,
        D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError>
            + Send
            + Sync
            + 'static,
        P: for<'a> Fn(PersistenceOperation<'a, M>) -> Result<R, RpcError> + Send + Sync + 'static,
        S: Fn(AppliedEffect<M>, R) -> Result<Value, RpcError> + Send + Sync + 'static,
    {
        self.methods.insert(
            CurrentMethod::Legacy(method.into()),
            Registered {
                requires,
                route: Route::Conditioned(crate::conditioned::route(
                    method, decoder, persist, sink,
                )),
                after_response: None,
            },
        );
        self
    }

    /// Register the one conditioned route whose successful result is owned by its connection.
    pub fn register_conditioned_attach<M, D, P>(
        &mut self,
        requires: Requirement,
        decoder: D,
        persist: P,
        cleanup: Arc<dyn Fn(dudes_types::AgentId, dudes_proto::rpc::pty::PtyId) + Send + Sync>,
    ) -> &mut Self
    where
        M: 'static,
        D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError>
            + Send
            + Sync
            + 'static,
        P: for<'a> Fn(
                PersistenceOperation<'a, M>,
            ) -> Result<dudes_proto::rpc::pty::PtyId, RpcError>
            + Send
            + Sync
            + 'static,
    {
        self.methods.insert(
            Method::AgentAttach.into(),
            Registered {
                requires,
                route: Route::Conditioned(crate::conditioned::attach_route(
                    decoder, persist, cleanup,
                )),
                after_response: None,
            },
        );
        self
    }

    /// Register the conditioned route that revokes its exact attachment before confirmed close.
    pub fn register_conditioned_detach<M, D, P>(
        &mut self,
        requires: Requirement,
        decoder: D,
        persist: P,
    ) -> &mut Self
    where
        M: 'static,
        D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError>
            + Send
            + Sync
            + 'static,
        P: for<'a> Fn(
                PersistenceOperation<'a, M>,
                Option<dudes_proto::rpc::pty::PtyId>,
            ) -> Result<Option<dudes_proto::PtyCloseOutcome>, RpcError>
            + Send
            + Sync
            + 'static,
    {
        self.methods.insert(
            Method::AgentDetach.into(),
            Registered {
                requires,
                route: Route::Conditioned(crate::conditioned::detach_route(decoder, persist)),
                after_response: None,
            },
        );
        self
    }

    /// Register the one conditioned route that consumes a connection's attachment ownership.
    ///
    /// `persist` returns only after the whole order is durable, and the outcome it hands
    /// back decides whether the route keeps or revokes that ownership.
    pub fn register_conditioned_write<M, D, P>(
        &mut self,
        requires: Requirement,
        decoder: D,
        persist: P,
    ) -> &mut Self
    where
        M: 'static,
        D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError>
            + Send
            + Sync
            + 'static,
        P: for<'a> Fn(
                PersistenceOperation<'a, M>,
            ) -> Result<dudes_proto::PtyWriteOutcome, RpcError>
            + Send
            + Sync
            + 'static,
    {
        self.methods.insert(
            Method::PtyWrite.into(),
            Registered {
                requires,
                route: Route::Conditioned(crate::conditioned::write_route(decoder, persist)),
                after_response: None,
            },
        );
        self
    }

    /// What `method` requires, if it is registered. For diagnostics and tests.
    #[must_use]
    pub fn requirement(&self, method: Method) -> Option<Requirement> {
        self.current_requirement(method.into())
    }

    /// What a current method requires, if it is registered.
    #[must_use]
    pub fn current_requirement(&self, method: CurrentMethod) -> Option<Requirement> {
        self.methods.get(&method).map(|entry| entry.requires)
    }

    /// The legacy methods registered, in table order. For diagnostics and tests.
    pub fn methods(&self) -> impl Iterator<Item = Method> + '_ {
        self.current_methods().filter_map(CurrentMethod::legacy)
    }

    /// All current methods registered, in table order.
    pub fn current_methods(&self) -> impl Iterator<Item = CurrentMethod> + '_ {
        self.methods.keys().copied()
    }

    /// Answer one frame.
    ///
    /// Every frame is answered, including one that is not JSON at all: that is an error response on
    /// a connection that **stays open**. Closing the connection instead would make a typo
    /// indistinguishable from a daemon that died, and would cost the client every call it had
    /// pipelined behind it.
    ///
    /// A frame that does not become a [`Request`] fails in one of two ways, and JSON-RPC 2.0 §5.1
    /// gives them different codes — as does the contract crate, which describes
    /// [`ErrorCode::ParseError`] as "the frame was not valid JSON" and
    /// [`ErrorCode::InvalidRequest`] as "the envelope was malformed". [`classify`] tells them apart.
    pub fn handle_line(
        &self,
        line: &str,
        provenance: PeerProvenance,
        outbound: &Arc<Outbound>,
    ) -> Response {
        let connection = ConditionedConnectionContext::without_peer(provenance);
        self.handle_line_on_connection(line, &connection, outbound)
    }

    pub(crate) fn handle_line_on_connection(
        &self,
        line: &str,
        connection: &ConditionedConnectionContext,
        outbound: &Arc<Outbound>,
    ) -> Response {
        self.handle_line_with_after_response_on_connection(line, connection, outbound)
            .into_response()
    }

    pub(crate) fn handle_line_with_after_response_on_connection<'a>(
        &'a self,
        line: &str,
        connection: &ConditionedConnectionContext,
        outbound: &Arc<Outbound>,
    ) -> DispatchOutcome<'a> {
        match serde_json::from_str::<Request>(line) {
            Ok(request) => {
                let replacement = if request.method == Method::GoalReplace.name() {
                    match crate::goal_replace::decode(line) {
                        Ok(params) => Some(params),
                        Err(err) => {
                            return DispatchOutcome::response(Response::err(request.id, err));
                        }
                    }
                } else {
                    None
                };
                self.dispatch_with_after_response_on_connection(
                    &request,
                    connection,
                    outbound,
                    replacement.as_ref(),
                )
            }
            Err(err) => {
                tracing::debug!(error = %err, "frame did not parse as a request");
                let (id, code, message) = classify(line);
                DispatchOutcome::response(Response::err(id, error(code, message)))
            }
        }
    }

    /// Answer one request.
    #[must_use]
    pub fn dispatch(
        &self,
        request: &Request,
        provenance: PeerProvenance,
        outbound: &Arc<Outbound>,
    ) -> Response {
        let connection = ConditionedConnectionContext::without_peer(provenance);
        self.dispatch_on_connection(request, &connection, outbound)
    }

    pub(crate) fn dispatch_on_connection(
        &self,
        request: &Request,
        connection: &ConditionedConnectionContext,
        outbound: &Arc<Outbound>,
    ) -> Response {
        self.dispatch_with_after_response_on_connection(request, connection, outbound, None)
            .into_response()
    }

    fn dispatch_with_after_response_on_connection<'a>(
        &'a self,
        request: &Request,
        connection: &ConditionedConnectionContext,
        outbound: &Arc<Outbound>,
        replacement: Option<&dudes_proto::rpc::goal::GoalReplaceParams>,
    ) -> DispatchOutcome<'a> {
        let provenance = connection.provenance();
        let id = request.id.clone();
        if request.jsonrpc != JSONRPC {
            return DispatchOutcome::response(Response::err(
                id,
                error(ErrorCode::InvalidRequest, "jsonrpc must be \"2.0\""),
            ));
        }

        if connection.transport_pid_is_unreadable()
            && request
                .method
                .parse::<CurrentMethod>()
                .ok()
                .and_then(|method| self.methods.get(&method))
                .is_some_and(|entry| {
                    matches!(
                        entry.requires,
                        Requirement::Agent | Requirement::OperatorOrAgent
                    )
                })
        {
            return DispatchOutcome::response(Response::err(
                id,
                error(ErrorCode::Unauthorized, "the call is not admitted"),
            ));
        }

        let presented = Presented::from_request(request);
        let role = match self
            .authorizer
            .authorize(presented, provenance, &request.method)
        {
            Ok(role) => role,
            Err(refusal) => {
                tracing::debug!(method = %request.method, "refused by the authorisation seam");
                return DispatchOutcome::response(Response::err(id, refused(&refusal)));
            }
        };

        let Ok(method) = request.method.parse::<CurrentMethod>() else {
            return DispatchOutcome::response(Response::err(
                id,
                error(ErrorCode::MethodNotFound, "no such method"),
            ));
        };
        let Some(entry) = self.methods.get(&method) else {
            return DispatchOutcome::response(Response::err(
                id,
                error(ErrorCode::MethodNotFound, "no such method"),
            ));
        };

        if !entry.requires.admits(&role) {
            tracing::debug!(method = %request.method, "refused by the method's requirement");
            return DispatchOutcome::response(Response::err(
                id,
                error(ErrorCode::Unauthorized, entry.requires.refusal()),
            ));
        }

        if method == CurrentMethod::Legacy(Method::GoalReplace) && replacement.is_none() {
            return DispatchOutcome::response(Response::err(
                id,
                error(
                    ErrorCode::InvalidParams,
                    "goal.replace requires original-frame validation",
                ),
            ));
        }
        let call = Call {
            goal_replacement: replacement,
            method,
            params: request.params.as_ref(),
            role: &role,
            outbound,
            hang_up: connection.hang_up(),
        };
        let result = match &entry.route {
            Route::Ordinary(handler) | Route::BoundedQuestion(handler) => handler(call),
            Route::Audited(handler) => handler(call, connection.peer_pid()),
            Route::Conditioned(route) => route(request.params.as_ref(), &role, connection),
        };
        match result {
            Ok(result) => DispatchOutcome {
                response: Response::ok(id, result),
                after_response: entry.after_response.as_deref(),
            },
            Err(err) => DispatchOutcome::response(Response::err(id, err)),
        }
    }
}

/// An [`RpcError`] with no typed `data`.
#[must_use]
pub fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code: code.code(),
        message: message.into(),
        data: None,
    }
}

/// The wire error a seam [`Refusal`] becomes.
///
/// Two codes, because classification produces two: the no-minting row in every provenance column, and
/// The retired-agent row outside its owner's tree, are `-32004 Refused{gate: token}`, and every other
/// refusing cell is `-32001 Unauthorized`. The `data`
/// on the `-32004` arm is the contract's typed [`RefusedData`], so a client reads the gate from a
/// field rather than from the message text.
#[must_use]
pub fn refused(refusal: &Refusal) -> RpcError {
    match refusal {
        Refusal::Unauthorized(unauthorized) => {
            error(ErrorCode::Unauthorized, unauthorized.reason.clone())
        }
        Refusal::Token => RpcError {
            code: ErrorCode::Refused.code(),
            message: refusal.to_string(),
            data: serde_json::to_value(RefusedData {
                gate: RefusalGate::Token,
            })
            .ok(),
        },
    }
}

/// The id a parse error is answered against when the frame named none this transport can read.
///
/// JSON-RPC 2.0 requires a *null* id here. [`Response::id`] is not nullable — the contract's
/// [`RequestId`] is a number or a string — so a frame whose id cannot be recovered is answered
/// against `0`. That is a deviation from the specification, recorded here rather than hidden:
/// answering is what keeps a client from waiting on a reply that never comes, and the alternative
/// (silence) turns a typo into a hang. Making the contract's response id nullable is a change to
/// `dudes-proto`, not to this crate.
pub const UNIDENTIFIED_REQUEST_ID: i64 = 0;

/// Sort a frame that did not become a [`Request`] into its error code, and recover the id it named.
///
/// The two classes are the specification's, and they are not interchangeable:
///
/// - **Not JSON at all** — [`ErrorCode::ParseError`], `-32700`. There is nothing to read an id
///   from, so the answer carries [`UNIDENTIFIED_REQUEST_ID`].
/// - **JSON that is not a request object** — [`ErrorCode::InvalidRequest`], `-32600`: a missing
///   `method`, a `params` of the wrong type, an id that is neither a number nor a string. The id is
///   usually readable here, so the client is answered against the call it is actually waiting on
///   rather than against a sentinel.
fn classify(line: &str) -> (RequestId, ErrorCode, &'static str) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return (
            RequestId::Number(UNIDENTIFIED_REQUEST_ID),
            ErrorCode::ParseError,
            "the frame was not valid JSON",
        );
    };
    let id = match value.get("id") {
        Some(Value::Number(n)) => n.as_i64().map(RequestId::Number),
        Some(Value::String(s)) => Some(RequestId::String(s.clone())),
        _ => None,
    };
    (
        id.unwrap_or(RequestId::Number(UNIDENTIFIED_REQUEST_ID)),
        ErrorCode::InvalidRequest,
        "the frame was not a valid request object",
    )
}

#[cfg(test)]
mod tests {
    use dudes_proto::rpc::system::PingResult;
    use dudes_types::Lsn;
    use serde_json::json;

    use super::*;
    use crate::auth::PingOnly;
    use crate::testing::AdmitAll;

    fn request(method: &str) -> Request {
        Request {
            jsonrpc: JSONRPC.to_owned(),
            id: RequestId::Number(1),
            method: method.to_owned(),
            params: None,
            v: None,
            cred: None,
        }
    }

    fn ping() -> PingResult {
        PingResult {
            version: "0.5.0".to_owned(),
            socket: "/run/duded.sock".into(),
            lsn: Lsn(7),
        }
    }

    fn code(response: &Response) -> i32 {
        response.error.as_ref().expect("an error response").code
    }

    /// A call from a caller with no peer to write to. Every test here asserts on the response, not
    /// on anything the connection carried.
    fn nowhere() -> Arc<Outbound> {
        Arc::new(Outbound::sink())
    }

    #[test]
    fn unreadable_transport_pid_refuses_current_agent_grants_before_authorization() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct CountingAuthorizer(Arc<AtomicUsize>);

        impl Authorizer for CountingAuthorizer {
            fn authorize(
                &self,
                _presented: Presented,
                _provenance: PeerProvenance,
                _method: &str,
            ) -> Result<PeerRole, Refusal> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(PeerRole::Unproven)
            }
        }

        let authorizations = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = Dispatcher::new(CountingAuthorizer(Arc::clone(&authorizations)));
        let connection = ConditionedConnectionContext::from_peer(
            crate::auth::PeerIdentity::unknown(),
            PeerProvenance::Unresolvable,
        );
        for requires in [Requirement::Agent, Requirement::OperatorOrAgent] {
            dispatcher.register_current_audited(CurrentMethod::TicketCheckout, requires, |_, _| {
                Ok(json!({"checkout": true}))
            });
            let response = dispatcher.dispatch_on_connection(
                &request("ticket.checkout"),
                &connection,
                &nowhere(),
            );
            assert_eq!(code(&response), ErrorCode::Unauthorized.code());
        }
        assert_eq!(authorizations.load(Ordering::SeqCst), 0);

        dispatcher.register_current_audited(
            CurrentMethod::TicketCheckout,
            Requirement::AnyRole,
            |call, pid| Ok(json!({"method": call.method.name(), "pid": pid})),
        );
        let response =
            dispatcher.dispatch_on_connection(&request("ticket.checkout"), &connection, &nowhere());
        assert_eq!(
            response.result.unwrap(),
            json!({"method": "ticket.checkout", "pid": null})
        );
        assert_eq!(authorizations.load(Ordering::SeqCst), 1);
    }

    /// The seam runs before the method table is consulted, so a refused peer cannot use the
    /// difference between `-32001` and `-32601` to enumerate the method surface.
    ///
    /// The two arms are the control: under `AdmitAll` the *same* unknown name yields
    /// `MethodNotFound`, so the `-32001` above is the seam speaking and not a transport that
    /// refuses everything.
    #[test]
    fn the_seam_runs_before_the_method_lookup() {
        let provenance = PeerProvenance::Unresolvable;

        let refusing = Dispatcher::new(PingOnly);
        assert_eq!(
            code(&refusing.dispatch(&request("no.such.method"), provenance, &nowhere())),
            ErrorCode::Unauthorized.code(),
            "an unknown name is refused, not reported as unknown"
        );

        let admitting = Dispatcher::new(AdmitAll);
        assert_eq!(
            code(&admitting.dispatch(&request("no.such.method"), provenance, &nowhere())),
            ErrorCode::MethodNotFound.code(),
            "once admitted, an unknown name is MethodNotFound"
        );
    }

    /// The requirement is checked after the seam admits, and it is what refuses.
    ///
    /// Both arms hold the seam, the request, the provenance and the handler constant and vary **only the
    /// requirement**: `AdmitAll` resolves `Unproven` in both, so arm A's `-32001` can only be the
    /// requirement speaking, and arm B shows the same registration reaching its handler when the
    /// requirement admits. Without arm B this would pass equally against a dispatcher that had
    /// stopped calling handlers at all.
    ///
    /// The other axis — the same requirement against a *different resolved role* — is varied over a
    /// real socket in `tests/seam.rs`, where the `Authorizer` that resolves an operator lives. It is
    /// deliberately unavailable here: `tests/no_operator_fake.rs` keeps `src/` free of any code that
    /// mints a role, and a `#[cfg(test)]` exception would be one that scan cannot see.
    #[test]
    fn the_requirement_is_what_refuses_once_the_seam_has_admitted() {
        let handler = |_: Call<'_>| Ok(json!({ "canned": "the handler ran" }));

        let mut requiring = Dispatcher::new(AdmitAll);
        requiring.register(Method::SystemDoctor, Requirement::Operator, handler);
        let refused = requiring.dispatch(
            &request("system.doctor"),
            PeerProvenance::Unresolvable,
            &nowhere(),
        );
        assert_eq!(
            code(&refused),
            ErrorCode::Unauthorized.code(),
            "arm A: the seam admitted, and the method's own requirement refused"
        );
        assert_eq!(
            refused.error.expect("an error").message,
            Requirement::Operator.refusal(),
            "and the reason names the requirement, not the seam"
        );

        let mut open = Dispatcher::new(AdmitAll);
        open.register(Method::SystemDoctor, Requirement::AnyRole, handler);
        let allowed = open.dispatch(
            &request("system.doctor"),
            PeerProvenance::Unresolvable,
            &nowhere(),
        );
        assert_eq!(
            allowed.result.expect("a result")["canned"],
            "the handler ran",
            "arm B: the identical registration reaches its handler when the requirement admits"
        );
    }

    /// The requirement is a set, not a ladder: every pairing of requirement and role is asserted,
    /// so a rewrite that made `Operator` "at least as privileged as" `Agent` fails here.
    #[test]
    fn every_requirement_admits_exactly_the_roles_it_names() {
        let roles = [
            PeerRole::Unproven,
            PeerRole::Operator,
            PeerRole::Agent {
                agent: dudes_types::AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            },
        ];
        for (requires, expected) in [
            (Requirement::AnyRole, [true, true, true]),
            (Requirement::Operator, [false, true, false]),
            (Requirement::Agent, [false, false, true]),
            (Requirement::OperatorOrAgent, [false, true, true]),
        ] {
            for (role, expected) in roles.iter().zip(expected) {
                assert_eq!(
                    requires.admits(role),
                    expected,
                    "{requires:?} against {}",
                    role.name()
                );
            }
        }
    }

    /// A method the contract defines but nobody registered is `MethodNotFound`, not a panic and
    /// not an internal error: the surface is what is registered, not what the enum can name.
    #[test]
    fn an_unregistered_contract_method_is_method_not_found() {
        let dispatcher = Dispatcher::new(AdmitAll);
        assert_eq!(
            code(&dispatcher.dispatch(
                &request("system.doctor"),
                PeerProvenance::Unresolvable,
                &nowhere()
            )),
            ErrorCode::MethodNotFound.code()
        );
    }

    /// The envelope is checked: a frame that is JSON-RPC-shaped but not version 2.0 is
    /// `InvalidRequest`.
    #[test]
    fn a_wrong_jsonrpc_version_is_invalid_request() {
        let mut req = request("system.ping");
        req.jsonrpc = "1.0".to_owned();
        let dispatcher = Dispatcher::new(AdmitAll);
        assert_eq!(
            code(&dispatcher.dispatch(&req, PeerProvenance::Unresolvable, &nowhere())),
            ErrorCode::InvalidRequest.code()
        );
    }

    /// A handler sees the role the seam resolved, and does not re-derive it. Asserted by having the
    /// handler return the role's name.
    #[test]
    fn the_resolved_role_reaches_the_handler() {
        let mut dispatcher = Dispatcher::new(PingOnly);
        dispatcher.register(Method::SystemPing, Requirement::AnyRole, |call| {
            Ok(json!({ "role": call.role.name(), "method": call.method.name() }))
        });
        let response = dispatcher.dispatch(
            &request("system.ping"),
            PeerProvenance::Unresolvable,
            &nowhere(),
        );
        let result = response.result.expect("a result");
        assert_eq!(result["role"], "unproven");
        assert_eq!(result["method"], "system.ping");
    }

    /// Every frame is answered, and the two ways a frame can fail to be a request get the two codes
    /// JSON-RPC 2.0 §5.1 gives them: `-32700` for text that is not JSON, `-32600` for JSON that is
    /// not a request object.
    ///
    /// The table asserts both classes rather than one, because the interesting mistake is
    /// collapsing them — a transport that answers `-32700` to everything passes any test that only
    /// checks the parse-error rows. The `expected_id` column carries the second half of the rule:
    /// an id is recovered wherever the frame named a readable one, so the client is answered
    /// against the call it is waiting on, and falls back to [`UNIDENTIFIED_REQUEST_ID`] only when
    /// there is nothing to read.
    #[test]
    fn a_frame_that_is_not_a_request_is_sorted_into_the_right_code() {
        let dispatcher = Dispatcher::new(AdmitAll);
        let provenance = PeerProvenance::Unresolvable;
        let unidentified = RequestId::Number(UNIDENTIFIED_REQUEST_ID);

        for (line, expected_code, expected_id) in [
            // Not JSON at all: nothing to parse, and no id to answer against.
            ("{not json", ErrorCode::ParseError, unidentified.clone()),
            ("", ErrorCode::ParseError, unidentified.clone()),
            ("[", ErrorCode::ParseError, unidentified.clone()),
            // JSON, but not a request object. The id is readable in the first two.
            (
                r#"{"jsonrpc":"2.0","id":42}"#,
                ErrorCode::InvalidRequest,
                RequestId::Number(42),
            ),
            (
                r#"{"id":"abc","method":7}"#,
                ErrorCode::InvalidRequest,
                RequestId::String("abc".into()),
            ),
            (
                r#"{"id":{"nested":1}}"#,
                ErrorCode::InvalidRequest,
                unidentified.clone(),
            ),
            ("[1,2,3]", ErrorCode::InvalidRequest, unidentified.clone()),
            ("null", ErrorCode::InvalidRequest, unidentified.clone()),
        ] {
            let response = dispatcher.handle_line(line, provenance, &nowhere());
            assert_eq!(
                code(&response),
                expected_code.code(),
                "`{line}` must be {expected_code:?}"
            );
            assert_eq!(response.id, expected_id, "id answered for `{line}`");
        }
    }

    /// A handler's result is returned verbatim against the request's id — the positive control for
    /// every "a response arrived" assertion elsewhere, which an echo server would also pass.
    #[test]
    fn a_handler_result_is_returned_against_the_request_id() {
        let expected = ping();
        let mut dispatcher = Dispatcher::new(PingOnly);
        let answer = expected.clone();
        dispatcher.register(Method::SystemPing, Requirement::AnyRole, move |_| {
            Ok(serde_json::to_value(&answer).unwrap())
        });

        let mut req = request("system.ping");
        req.id = RequestId::String("seven".to_owned());
        let response = dispatcher.dispatch(&req, PeerProvenance::Unresolvable, &nowhere());

        assert_eq!(response.id, RequestId::String("seven".to_owned()));
        let got: PingResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(got, expected);
    }
}
