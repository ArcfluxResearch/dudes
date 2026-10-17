//! The Dudes control transport: newline-delimited JSON-RPC 2.0 over a Unix socket.
//!
//! Everything that talks to the daemon shares this one door — the operator CLI, an agent's own
//! calls, the hook shim, and later the floor. This crate owns framing, dispatch, the authorisation
//! seam and the conditioned-effect router; domain behaviour remains injected.
//!
//! # Ordinary calls and conditioned effects are different routes
//!
//! [`methods`] carries nine methods — eight operator ones and `agent.ask`, which is agent-only —
//! and not one of their handlers knows what the daemon does. Their behaviour arrives as two ports
//! — [`Control`] for what changes state, [`JournalRead`] for what reads it. A third,
//! [`ConditionedAgentControl`], persists `agent.answer` and `agent.steer` after the conditioned
//! router decides them. Separately, the seven
//! provenance-conditioned methods cross [`conditioned`], which centrally decodes, derives the
//! target, computes and consumes a pure disposition, and persists through an injected adapter.
//! Only that adapter sees the applied provenance and router-owned actor; a sink receives an
//! identity-opaque, private-field [`AppliedEffect`]. The daemon owns the run-local
//! [`OperatorAuthoritySource`]; this crate can only bind one of its leases to an accepted
//! connection. No production effect sink exists here. Which roles a method admits is
//! [`Requirement`], declared at registration and checked first on the dispatch path.
//!
//! [`tail::Subscribers`] is the other half of that: this crate fans `journal.appended` out to
//! whoever is subscribed, and the daemon calls it after an append is durable. Nothing here watches
//! the journal.
//!
//! # This crate links no async runtime
//!
//! Not optionally, and not behind a feature. Invariant 9 says `dudes-hook` links no async runtime,
//! no TLS and no subscriber, and the shim reaches the daemon through this transport. A feature flag
//! cannot buy that back: the layering gate walks `cargo metadata --all-features` over every
//! workspace member, so an optional dependency behind *any* feature of *any* member is an edge in
//! the graph the gate reads. The rule is therefore "never name a runtime here", the accept loop is a
//! thread per connection, and the runtime lives in the daemon binary — which is the only place it
//! is linked at all.
//!
//! # Identity is a seam here, and one classifier fills it
//!
//! Invariant 8: identity is what a peer *presents*, qualified by what the daemon can independently
//! *resolve* about that peer. [`Authorizer`] keeps those two as separate inputs for exactly that
//! reason, and every dispatch passes through one call to it — one function, not a check per
//! handler.
//!
//! [`PeerClassifier`] implements peer classification: default-deny ([`PeerRole::Unproven`] for a peer that
//! presents nothing), the operator credential on the [`Request`](dudes_proto::rpc::Request)
//! envelope, an owned peer refused the operator table, and a token honoured only from inside its
//! own agent's tree. Every cell is decided in [`auth::classify`] and nowhere else.
//!
//! **Two of its inputs are ports, and neither has a live implementation here.**
//! [`auth::PeerResolver`] would read the process table, which the layering gate forbids this crate
//! (and which may not hold on every platform); [`auth::MintedTokens`] is a journal
//! projection, which invariant 1 puts in the supervisor's `World` rather than in a second fold
//! here. So a daemon built on this crate is only as classified as the resolver it supplies, and
//! [`PingOnly`] remains what a daemon runs before it has either.
//!
//! A transport that cannot report a peer pid is
//! refused before an agent-callable method, and no token resolves on its hash alone.
//!
//! # Transport
//!
//! [`serve`] is generic over [`Listener`] and [`Connection`], and [`PeerIdentity`]'s fields are
//! optional so a failed credential read is representable rather than a panic. The implementation
//! shipped is [`LocalTransport`]: a Unix socket or Windows named pipe. Invariant 10 forbids a
//! listener outside the local machine, and this crate binds no network address.

pub mod auth;
pub mod client;
pub mod codec;
pub mod conditioned;
pub mod dispatch;
pub mod endpoint;
pub mod methods;
pub mod outbound;
#[cfg(unix)]
pub mod peer;
pub mod server;
pub mod system;
pub mod tail;
// Off in a default build. `src/testing.rs`'s header owns the reason; `cfg(test)` is here because
// this crate's own unit tests reach the fakes through `crate::testing`, and a bare feature gate
// would not compile.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use auth::{
    Authorizer, MintedTokens, PeerClassifier, PeerIdentity, PeerPid, PeerProvenance, PeerResolver,
    PeerRole, PingOnly, Presented, PresentedRow, Refusal, Requirement, TokenOwner, Unauthorized,
    classify,
};
pub use client::{Client, ClientError, Inbound, LocalClient};
pub use codec::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame};
pub use conditioned::{
    AppliedEffect, AppliedPersistence, DecodedEffect, MAX_AGENT_ANSWER_TEXT_BYTES,
    MAX_AGENT_STEER_TEXT_BYTES, MAX_MAIL_SEND_BODY_BYTES, MAX_PTY_WRITE_BYTES,
    OperatorAuthoritySource, PersistenceOperation, PersistenceReceipt, R5Method,
};
pub use dispatch::{Call, Dispatcher, Handler};
pub use endpoint::local_endpoint;
pub use methods::{
    ConditionedAgentControl, ConditionedAttachControl, ConditionedMemoryControl,
    ConditionedPtyControl, Control, JournalRead, register_agent_attach, register_agent_effects,
    register_memory_effect, register_pty_write,
};
pub use outbound::{Outbound, SendError};
pub use server::{
    Connection, CredentialCreationError, Halt, HangUp, Listener, OPERATOR_CREDENTIAL_NAME,
    StopFlag, serve, serve_connection, serve_with_authority,
};
// Local transport names are split from Unix file modes. The transport and credential
// name are available on every supported platform; mode accessors remain Unix-only because a mode
// has no Windows meaning.
pub use server::LocalTransport;
#[cfg(unix)]
pub use server::{operator_credential_permissions, runtime_dir_permissions, socket_permissions};
pub use tail::Subscribers;

mod goal_replace;
mod recovery;
