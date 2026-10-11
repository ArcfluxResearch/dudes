//! The daemon's side of the transport's three ports, and the handle every other thread reaches the
//! loop through.
//!
//! # One writer, one thread
//!
//! Invariant 1 has a runtime shape as well as a storage one: there is exactly one `Supervisor`, one
//! `Writer` and one thread that touches either. Everything else — the accept loop, the reaper, each
//! predicate — posts a [`Job`] and, where it needs an answer, waits for one. That is what makes the
//! decide loop's ordering contract keepable: `Input::Persisted` acknowledgements are folded in
//! emission order because one thread emits and acknowledges them.
//!
//! # State-changing ports return only after the record is durable
//!
//! `Control` and `ConditionedAgentControl` make that the implementor's obligation, and this is
//! where it is discharged:
//! [`Handle::ask`] blocks until the loop has run the command **to quiescence**, which means every
//! `Persist` it produced has been appended — and `Writer::append` returns only after the fsync. So
//! a door that answered `ok` is a door whose record a crash cannot take back.
//!
//! # `JournalRead` needs no loop at all
//!
//! It is read-only and reads the journal's own files, so it opens its own `Reader` and never
//! contends with the writer. `Reader::new` is the follow-a-live-writer constructor — the directory
//! has already been repaired by `open()` in startup step 2 — and each call reads afresh, which is
//! the shape `JournalRead::entries_from`'s own doc argues for: an iterator held open across a
//! rotation silently yields a rotation-dependent suffix.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dudes_journal::Reader;
use dudes_proto::rpc::AgentRef;
use dudes_proto::rpc::agent::{
    AttachParams, HireParams, HireResult, LookParams, McpParams, McpResult, RenameParams,
    RetireParams, WhyResult,
};
use dudes_proto::rpc::board::{
    CardClaimParams, CardCreateParams, CardCreateResult, CardListParams, CardTransitionParams,
};
use dudes_proto::rpc::goal::{GoalSetParams, GoalSetResult};
use dudes_proto::rpc::memory::MemoryAppendParams;
use dudes_proto::rpc::memory::{MemoryReadResult, MemorySearchOptions, MemorySearchResult};
use dudes_proto::rpc::pty::{PtyId, PtyWriteParams};
use dudes_proto::rpc::system::AgentHealth;
use dudes_proto::rpc::{AgentText, ErrorCode, RpcError};
use dudes_proto::{
    Actor, AgentView, CardView, ConditionedMethod, ConditionedTarget, Entry, Event, Evidence,
    HookReply, PayloadDigest, PeerProvenance, PtyWriteOutcome, Record,
};
use dudes_providers::launch::LaunchSpec;
use dudes_rpc::PersistenceOperation;
use dudes_rpc::dispatch::error;
use dudes_rpc::methods::{
    ConditionedAgentControl, ConditionedAttachControl, ConditionedDetachControl,
    ConditionedMemoryControl, ConditionedPtyControl, Control, JournalRead,
};
use dudes_supervisor::{HookId, KeyHealth, RuntimeSignal};
use dudes_types::{AgentId, GoalId, Lsn};

use crate::pty_output::OutputSubscription;

/// The daemon's neutral port to the concrete attached PTY owner.
pub trait PtyHostPort: Send {
    /// Open one typed provider launch at the requested dimensions.
    ///
    /// # Errors
    ///
    /// The launch did not open, and whether its cleanup is confirmed.
    fn open(&mut self, spec: LaunchSpec, cols: u16, rows: u16)
    -> Result<OpenedPty, PtyOpenFailure>;
    /// Write the exact caller slice to one handle **once**, and answer with the host's byte count.
    ///
    /// The count is the whole answer: the caller compares it against the slice it passed and
    /// derives `Complete` or `Partial` from that comparison. An implementation therefore neither
    /// retries a short write nor reports one as success — the host is invoked once per
    /// request and journals its confirmed outcome, and a retry hidden here would make "once" a
    /// property of nobody's code.
    fn write(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize>;
    /// Change one live handle's character-cell size.
    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()>;
    /// Take the one subscription to a live handle's output.
    ///
    /// `None` when the host does not own that handle or its one subscription was already taken.
    fn subscribe(&mut self, pty: PtyId) -> Option<OutputSubscription>;
    /// Close one exact handle.
    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome;
    /// Attempt every session still retained by the host.
    fn shutdown(&mut self) -> io::Result<()>;
}

/// One attached launch the host opened and owns.
#[derive(Debug)]
pub struct OpenedPty {
    /// The live handle.
    pub pty: PtyId,
    /// The child it runs.
    pub child: PtyChild,
}

/// The child an attached PTY runs, as the process relation registers it.
#[derive(Debug)]
pub struct PtyChild {
    /// The child's pid, which the PTY host holds unreaped until the PTY is closed.
    pub pid: u32,
    /// A duplicate of the Job that contains the child's tree, for the relation to hold. `None`
    /// when the host could not duplicate it, which refuses the registration.
    #[cfg(windows)]
    pub job: Option<std::os::windows::io::OwnedHandle>,
}

/// Why one attached launch did not open, preserving cleanup uncertainty.
#[derive(Debug)]
pub enum PtyOpenFailure {
    /// No host-owned session remains.
    Failed(io::Error),
    /// The host retains a session whose absence it could not prove.
    CleanupUnconfirmed {
        /// Reserved handle retained by the host.
        pty: PtyId,
        /// The open failure.
        cause: io::Error,
        /// The failed cleanup.
        cleanup: io::Error,
    },
}

/// Result of closing one exact attached handle.
#[derive(Debug)]
pub enum PtyCloseOutcome {
    /// The live entry was closed.
    Closed,
    /// The host no longer owns it.
    AlreadyClosed,
    /// Ownership could not be confirmed absent.
    Failed(io::Error),
}

/// A closed PTY port for daemon tests that exercise no attached capability.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ClosedPtyHost;

impl PtyHostPort for ClosedPtyHost {
    fn open(
        &mut self,
        _spec: LaunchSpec,
        _cols: u16,
        _rows: u16,
    ) -> Result<OpenedPty, PtyOpenFailure> {
        Err(PtyOpenFailure::Failed(io::Error::other(
            "the test PTY host is closed",
        )))
    }

    fn write(&mut self, _pty: PtyId, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the test PTY host is closed"))
    }

    fn resize(&mut self, _pty: PtyId, _cols: u16, _rows: u16) -> io::Result<()> {
        Err(io::Error::other("the test PTY host is closed"))
    }

    fn subscribe(&mut self, _pty: PtyId) -> Option<OutputSubscription> {
        None
    }

    fn close(&mut self, _pty: PtyId) -> PtyCloseOutcome {
        PtyCloseOutcome::AlreadyClosed
    }

    fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Why the daemon is stopping.
///
/// Recorded rather than collapsed, so that a test can say *which* exit path it drove and a log
/// line can say which one happened. The first three are requested exits; the last two are faults rather
/// than requests, and they are separate for the same reason — a daemon that stopped because its own
/// machinery broke must not be indistinguishable from one an operator asked to stop, and two
/// different pieces of broken machinery must not be indistinguishable from each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCause {
    /// The `system.shutdown` RPC.
    ShutdownRpc,
    /// `SIGTERM` (or `SIGINT`).
    Signal,
    /// The loop ran out of work to serve: every sender of its job channel is gone. This is the
    /// `main`-returning path, and it is the one an in-process test drives by dropping its handle.
    Ended,
    /// The peer resolver panicked, so no further connection can be given a provenance.
    ///
    /// Nobody asked for this one. The alternative to naming it is a daemon that keeps running with
    /// a resolver it can no longer ask — answering some peers and silently failing to identify the
    /// rest — which is the state invariant 8 exists to forbid. `classify::GuardedResolver` is what
    /// turns the panic into this.
    ResolverPanicked,
    /// The accept loop panicked, so the door is gone.
    ///
    /// Nobody asked for this one either, and it is a different fault from [`Self::ResolverPanicked`]
    /// rather than the same one seen twice. A resolver's panic leaves its own connection unserved
    /// and the loop still accepting; a panic in the accept loop's *own* frame — `Listener::accept`,
    /// the halt handle, admission, or the `scope.spawn` the OS refuses a thread for — unwinds the
    /// loop itself, and the listener share it held goes with it. What is left is a daemon no socket
    /// call can reach, which therefore cannot be told to stop and cannot kill its children:
    /// invariant 10's daemon-exit half with nothing but a signal left to reach it.
    ///
    /// `boot::start_accept` is what turns the panic into this, on the principle [`Self::Ended`]
    /// already applies to a loop that *returned* unbidden.
    AcceptLoopPanicked,
    /// The ready Floor listener ended without a requested stop.
    FloorGatewayEnded,
}

/// One `ticket.*` request, after the router resolved who is asking. Operator-only: `by` is
/// never a request field.
#[derive(Debug)]
pub enum TicketAsk {
    /// `ticket.create`.
    Create {
        /// The new ticket's fields.
        request: dudes_proto::rpc::ticket::TicketCreateParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `ticket.edit`.
    Edit {
        /// The conditional change.
        request: dudes_proto::rpc::ticket::TicketEditParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `ticket.assign`.
    Assign {
        /// The conditional assignment.
        request: dudes_proto::rpc::ticket::TicketAssignParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `ticket.remove`.
    Remove {
        /// The conditional removal.
        request: dudes_proto::rpc::ticket::TicketRemoveParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `ticket.accept`.
    Accept {
        /// The conditional acceptance.
        request: dudes_proto::rpc::ticket::TicketAcceptParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `ticket.read`.
    Read(dudes_proto::rpc::ticket::TicketReadParams),
}

/// A command that changes state, waiting for the loop.
///
/// **One variant per RPC method, named after it**, so that reading `register_all` and reading this
/// enum give the same list. `agent.ask`'s variant is therefore `Ask`, inside `enum Ask` — which is
/// what the lint below objects to, and renaming it would be the one variant here that does not
/// name its method.
#[expect(
    clippy::enum_variant_names,
    reason = "the variants name their methods; agent.ask's is `ask`, and a variant renamed to \
              satisfy the lint would be the only one a reader could not map back to a row"
)]
#[derive(Debug)]
pub enum Ask {
    /// Read or replace the next managed launch's desired server names.
    Mcp(McpParams),
    /// `agent.hire`.
    Hire(Box<HireParams>),
    /// `agent.retire`.
    Retire(Box<RetireParams>),
    /// `goal.set`: its admission, then its check's run before the first turn, or, for a check
    /// the operator says may already pass, the append itself.
    SetGoal(Box<GoalSetParams>),
    /// `goal.set` once its check ran and did not pass: the whole admission again, then the
    /// append, unless the caller has gone.
    SetGoalChecked {
        /// The request, as first admitted.
        params: Box<GoalSetParams>,
        /// What the run saw.
        baseline: dudes_proto::CheckBaseline,
        /// The run's count, which this job ends.
        run: u64,
        /// Whether the caller has gone.
        hang_up: dudes_rpc::HangUp,
    },
    /// `goal.replace`: its admission, then its check's run, or the append for a check that may
    /// already pass.
    ReplaceGoal(Box<dudes_proto::rpc::goal::GoalReplaceParams>),
    /// `goal.replace` once its check ran and did not pass: its admission checks again, then the append.
    ReplaceGoalChecked {
        /// The request, as first admitted.
        params: Box<dudes_proto::rpc::goal::GoalReplaceParams>,
        /// What the run saw.
        baseline: dudes_proto::CheckBaseline,
        /// The run's count, which this job ends.
        run: u64,
    },
    /// `loop.pause`.
    Pause(AgentId),
    /// `loop.resume`.
    Resume(AgentId),
    /// `loop.halt`.
    Halt(AgentId),
    /// `agent.ask`. The agent is the one the seam resolved from the caller's token, never a
    /// params field.
    Ask {
        /// Who asked.
        agent: AgentId,
        /// The question.
        text: String,
    },
    /// `board.create`.
    BoardCreate {
        /// The new card's fields.
        request: CardCreateParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `board.claim`.
    BoardClaim {
        /// The conditional claim.
        request: CardClaimParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `board.transition`.
    BoardTransition {
        /// The conditional status move.
        request: CardTransitionParams,
        /// Caller identity resolved by the router.
        by: Actor,
    },
    /// `board.list`.
    BoardList(CardListParams),
    /// `ticket.*`.
    Ticket(TicketAsk),
    /// `agent.list`.
    AgentList,
    /// A conditioned `agent.answer` that the router selected to apply.
    Answer {
        /// The target derived from typed params.
        agent: AgentId,
        /// The answer.
        text: String,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
    },
    /// A conditioned `agent.steer` that the router selected to apply.
    Steer {
        /// The target derived from typed params.
        agent: AgentId,
        /// Context for the next governed event.
        text: String,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
    },
    /// An applied connection-bound `agent.attach`.
    Attach {
        /// The target derived from typed params.
        agent: AgentId,
        /// The router-derived actor.
        by: dudes_proto::Actor,
        /// The accepted connection's exact provenance.
        provenance: PeerProvenance,
        /// Requested terminal columns.
        cols: u16,
        /// Requested terminal rows.
        rows: u16,
    },
    /// An applied `agent.detach` after the router revoked the exact connection binding.
    Detach {
        /// The bound agent.
        agent: AgentId,
        /// The bound PTY, derived from the connection rather than params.
        pty: PtyId,
        /// The router-derived peer actor.
        by: dudes_proto::Actor,
        /// The exact resolved peer provenance.
        provenance: PeerProvenance,
    },
    /// An applied `pty.write` the conditioned router selected and whose ownership it checked.
    ///
    /// The bytes are the decoder's exact decoded sequence, carried rather than re-derived: the
    /// digest the loop journals and the bytes the loop writes must be of one value, and a second
    /// decode here would be a second chance for them to differ.
    PtyWrite {
        /// The handle the requesting connection owns.
        pty: PtyId,
        /// The exact decoded bytes.
        bytes: Vec<u8>,
        /// The router's digest of exactly those bytes.
        ///
        /// Carried rather than recomputed here, as `MemoryAppendQuarantined`'s is: the router
        /// derived it from the same value the decoder produced, so the record names the bytes that
        /// reach the host. A second hash on this side would be a second chance for the digest and
        /// the write to describe different things, and would put a hashing dependency inside the
        /// daemon binary to buy it.
        payload: PayloadDigest,
        /// The router-derived actor (invariant 8).
        actor: dudes_proto::Actor,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
    },
    /// A resize of the exact PTY an attached agent holds. No record is journaled for it.
    PtyResize {
        /// The attached agent.
        agent: AgentId,
        /// The handle the caller was given for that attachment.
        pty: PtyId,
        /// Requested terminal columns.
        cols: u16,
        /// Requested terminal rows.
        rows: u16,
    },
    /// An applied `memory.append` selected by the conditioned router.
    MemoryAppend {
        /// The centrally derived target.
        agent: AgentId,
        /// The exact decoded UTF-8 bytes.
        payload: Vec<u8>,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
    },
    /// A read whose target and optional caller came from the authenticated role.
    MemoryRead {
        /// The memory owner.
        agent: AgentId,
        /// The admitted agent caller, absent for operators.
        caller: Option<AgentId>,
        /// Exclusive append cursor.
        since_lsn: Option<Lsn>,
    },
    /// A search with validated options and a target derived from the authenticated role.
    MemorySearch {
        /// Memory owner.
        agent: AgentId,
        /// Resolved agent caller; absent for an operator.
        caller: Option<AgentId>,
        /// Validated query, exclusive candidate cursor and hit limit.
        options: MemorySearchOptions,
    },
    /// A public roster read from an authenticated caller.
    TeamRoster {
        /// The resolved agent caller, absent for an operator.
        caller: Option<AgentId>,
    },
    /// A digest-only `memory.append` quarantine selected by the conditioned router.
    MemoryAppendQuarantined {
        /// The centrally derived target.
        agent: AgentId,
        /// Digest of the exact decoded UTF-8 bytes.
        payload: PayloadDigest,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
        /// The table's stable reason.
        reason: String,
    },
    /// One checked browser terminal effect.
    BrowserTerminal(crate::floor_terminal::BrowserAppliedTerminalEffect),
    /// A conditioned call withheld by the disposition table.
    ConditionedEffectSuppressed {
        /// Which method was withheld.
        method: ConditionedMethod,
        /// The centrally derived target.
        target: ConditionedTarget,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
        /// The table's stable reason.
        reason: String,
        /// Digest of the decoded payload, when present.
        payload: Option<PayloadDigest>,
    },
    /// `agent.rename`.
    Rename(Box<RenameParams>),
    /// `agent.look`.
    Look(Box<LookParams>),
    /// Step 1 of a worktree hire: admit it, and name the lease it starts from, if any.
    WorktreeAdmit(Box<HireParams>),
    /// Step 3: admit again, then journal the lease before `git worktree add` runs.
    WorktreeLease {
        /// The hire's own params, checked again.
        params: Box<HireParams>,
        /// The repository the tree is added from, as git reported it.
        origin: PathBuf,
        /// The commit the branch starts at, resolved once.
        base: dudes_types::GitCommit,
        /// The lease it is based on.
        from: Option<dudes_types::WorktreeId>,
    },
    /// Step 5: admit again, then hire into the tree that was just created.
    WorktreeHire {
        /// The hire's own params.
        params: Box<HireParams>,
        /// The pending lease whose tree this agent is hired into.
        worktree: dudes_types::WorktreeId,
    },
    /// Step 2b: refuse a repository that is at, under or above a live lease, before any git runs
    /// in it.
    WorktreeCheckOrigin {
        /// The hire's own params, checked again.
        params: Box<HireParams>,
        /// The repository git reported.
        origin: PathBuf,
    },
    /// A lease, if it is still pending.
    WorktreeInfo(dudes_types::WorktreeId),
    /// End a pending lease whose tree is gone.
    WorktreeNeverAssigned(dudes_types::WorktreeId),
    /// Every lease that is pending now.
    WorktreePending,
    /// `worktree.list`.
    WorktreeEntries,
    /// `worktree.removed`.
    WorktreeRemoved(dudes_types::WorktreeId),
}

/// What the loop answered.
#[derive(Debug)]
pub enum Answered {
    /// The acknowledged desired set, with whether this call appended a change.
    Mcp(McpResult),
    /// The new agent.
    Hired(HireResult),
    /// The new goal.
    GoalSet(GoalSetResult),
    /// A local goal replacement outcome.
    GoalReplaced(dudes_proto::rpc::goal::GoalReplaceResult),
    /// The goal's check is running once before the first turn; the caller waits on it.
    CheckRunning(Box<crate::baseline::Ticket>),
    /// The created card.
    CardCreated(CardCreateResult),
    /// The created ticket.
    TicketCreated(dudes_proto::rpc::ticket::TicketCreateResult),
    /// Where an assigned ticket stands.
    TicketAssigned(dudes_proto::rpc::ticket::TicketAssignResult),
    /// One ticket in full.
    TicketRead(Box<dudes_proto::rpc::ticket::TicketReadResult>),
    /// Current cards matching a board list filter.
    Cards(Vec<CardView>),
    /// Every hired, unretired agent.
    Agents(Vec<AgentView>),
    /// The exact acknowledged journal position for `memory.append`.
    MemoryAppended(Lsn),
    /// A bounded applied-memory page.
    MemoryRead(MemoryReadResult),
    /// One bounded memory search page.
    MemorySearch(MemorySearchResult),
    /// Current public colleagues.
    Roster(Vec<dudes_proto::rpc::team::RosterEntry>),
    /// The exact host handle opened for `agent.attach`.
    Attached(PtyId),
    /// The PTY a browser attach opened, for its channel.
    BrowserAttached(crate::floor_terminal::BrowserAttachment),
    /// The host's confirmed close outcome after both close records are durable.
    PtyClosed(dudes_proto::PtyCloseOutcome),
    /// The host's confirmed byte outcome for `pty.write`, with both records already durable.
    PtyWritten(dudes_proto::PtyWriteOutcome),
    /// A worktree hire was admitted, with the lease it is based on when it names one.
    WorktreeAdmitted(Option<crate::worktree::hire::LeaseView>),
    /// The lease journaled for a worktree hire.
    WorktreeLeased(crate::worktree::hire::LeaseView),
    /// A lease, when it is pending.
    WorktreeInfo(Option<crate::worktree::hire::LeaseView>),
    /// The pending leases.
    WorktreeList(Vec<crate::worktree::hire::LeaseView>),
    /// Every lease that is not removed, with its path.
    WorktreeEntries(Vec<dudes_proto::rpc::worktree::WorktreeEntry>),
    /// Nothing to report; any record the command appended is durable.
    Done,
}

/// One loop-owned `system.doctor` snapshot.
///
/// The lsn travels with the health because the accept thread must not read the separately
/// published lsn after this snapshot leaves the loop. Another job may advance that publication
/// first, which would pair newer journal state with older diagnostic state in one reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorSnapshot {
    /// The newest journal lsn when the loop captured the diagnostic fields.
    pub lsn: Lsn,
    /// The failed append position if the writer has stopped accepting writes.
    pub journal_stopped: Option<dudes_proto::rpc::JournalStoppedData>,
    /// The adoption-key diagnostics captured on the same loop turn.
    pub health: KeyHealth,
    /// Replay-derived per-agent diagnostics captured in the same loop turn.
    pub agents: Vec<AgentHealth>,
    /// Every provider with an open turn on the same loop turn, whose sign-in is not checked.
    pub busy: std::collections::BTreeSet<dudes_types::Provider>,
}

/// A writer reply carrying internal provenance for a no-write answer under storage poison.
#[doc(hidden)]
#[derive(Debug)]
pub struct HookAnswer {
    pub(crate) reply: HookReply,
    pub(crate) stopped_at: Option<Lsn>,
}

#[cfg(feature = "testing")]
impl HookAnswer {
    /// The reply produced by the writer, without a socket timeout fallback.
    #[doc(hidden)]
    #[must_use]
    pub fn into_reply_for_test(self) -> HookReply {
        self.reply
    }
}

impl From<HookReply> for HookAnswer {
    fn from(reply: HookReply) -> Self {
        Self {
            reply,
            stopped_at: None,
        }
    }
}

/// One unit of work for the loop thread.
#[derive(Debug)]
pub enum Job {
    /// Stop one exact owned child whose bounded protocol failed.
    ProtocolFailed(dudes_supervisor::host::AdoptionKey),
    /// A private typed closing operation, after transport authentication.
    Closing(Box<ClosingJob>),
    /// One authenticated finite browser hire/briefing intent.
    Onboard {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_actions::OnboardRequest,
        /// The truthful, bounded result, or the hire's check running.
        reply: Sender<crate::floor_actions::OnboardStep>,
    },
    /// The browser hire once its check ran and did not pass: the hire's admission again, then
    /// its append, or for a retried partial attempt, the goal's append.
    OnboardChecked {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_actions::OnboardRequest,
        /// What the run saw.
        baseline: dudes_proto::CheckBaseline,
        /// The run's count, which this job ends.
        run: u64,
        /// The original HTTP request's response channel.
        reply: Sender<crate::floor_actions::OnboardStep>,
    },
    /// Resume one durable partial hire in a later writer turn.
    OnboardContinue {
        /// The same admitted session, checked again at the goal append.
        session: crate::floor_session::SessionIdentity,
        /// The exact original request fields.
        request: crate::floor_actions::OnboardRequest,
        /// The original HTTP request's response channel.
        reply: Sender<crate::floor_actions::OnboardStep>,
    },
    /// One authenticated Floor answer to the open question it names.
    Answer {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_answer::AnswerRequest,
        /// The truthful, bounded result.
        reply:
            Sender<Result<crate::floor_answer::AnswerOutcome, crate::floor_answer::AnswerRefusal>>,
    },
    /// One authenticated Floor pause or resume of the agent it names.
    Control {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields, and the route's verb.
        request: crate::floor_control::ControlRequest,
        /// The truthful, bounded result.
        reply: Sender<
            Result<crate::floor_control::ControlOutcome, crate::floor_control::ControlRefusal>,
        >,
    },
    /// One authenticated Floor change or renewal of the goal of the agent it names.
    Goal {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_goal::GoalRequest,
        /// The truthful, bounded result, or the change's check running.
        reply: Sender<crate::floor_goal::GoalStep>,
    },
    /// A browser goal change once its check ran and did not pass: the admission again, then the
    /// append.
    GoalChecked {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_goal::GoalRequest,
        /// What the run saw.
        baseline: dudes_proto::CheckBaseline,
        /// The run's count, which this job ends.
        run: u64,
        /// The truthful, bounded result.
        reply: Sender<crate::floor_goal::GoalStep>,
    },
    /// What the journal records about one agent's tree, for the Floor's Git read.
    GitTarget {
        /// The agent the Floor asked about.
        agent: AgentId,
        /// What to read or answer, or `None` when the journal has nothing to read for this id.
        reply: Sender<Option<crate::floor_git::GitTarget>>,
    },
    /// One read of an agent's memory for the Floor's Memory tab, from recorded values only.
    FloorMemory {
        /// The agent and what to read of its memory.
        request: crate::floor_memory::MemoryRequest,
        /// What the projection holds. Dropped unanswered at shutdown.
        reply: Sender<crate::floor_memory::MemoryAnswer>,
    },
    /// One authenticated Floor retirement of the agent it names.
    Retire {
        /// Registry-derived admitting session.
        session: crate::floor_session::SessionIdentity,
        /// Strictly parsed browser fields.
        request: crate::floor_retire::RetireRequest,
        /// The truthful, bounded result.
        reply:
            Sender<Result<crate::floor_retire::RetireOutcome, crate::floor_retire::RetireRefusal>>,
    },
    /// Capture an authenticated, session-specific Floor snapshot in writer order.
    FloorSubscribe {
        /// The registry-derived session, never a browser-supplied identity field.
        session: crate::floor_session::SessionIdentity,
        /// The captured snapshot and stream subscription.
        reply: Sender<Result<crate::floor_stream::Opened, String>>,
    },
    /// Decide on one read of the operator file.
    EnrollmentReload {
        /// What the connection thread read, hashed and parsed.
        read: Box<crate::enrollment_reload::ReloadRead>,
        /// The method's result, or its refusal.
        reply: Sender<Result<dudes_proto::rpc::floor::EnrollmentReloadResult, RpcError>>,
    },
    /// The add-only resync window ended.
    EnrollmentResync {
        /// The window that scheduled this wake.
        window_id: u64,
    },
    /// Feed the machine.
    Input(Box<dudes_supervisor::Input>),
    /// One predicate run ended, with the verdict it observed or with none. Sent exactly once per
    /// run by its thread, so the loop clears that run's mark in the same job that hands the
    /// verdict to the machine.
    PredicateEnded {
        /// The goal the run judged.
        goal: GoalId,
        /// Whether it held, and what it rested on; `None` for a run that observed no verdict.
        verdict: Option<(bool, Evidence)>,
    },
    /// One new goal's check run before its first turn ended. Sent by the run's own thread when
    /// it ends, so the loop stops counting it as live work beside the agent.
    BaselineEnded {
        /// The run's count among the loop's runs.
        id: u64,
    },
    /// One memory-condense model-summary call ended, with the plan it was admitted for and
    /// whatever its one-shot child reported. Sent exactly once per call by its own thread, never
    /// by the loop thread (mirrors [`Job::PredicateEnded`]'s one-send rule).
    MemoryCondenseModelEnded {
        /// The agent whose memory this call attempted to condense.
        agent: AgentId,
        /// The admitted plan, snapshotted before the child was spawned. A predecessor that moved
        /// on while the call was in flight makes publication a safe no-op.
        plan: dudes_journal::CondensePlan,
        /// What the daemon observed, or `None` for a call this runner could not even start.
        outcome: Option<crate::predicate::PredicateCaptured>,
    },
    /// The version probe of the installed Pi ended. Sent once per probe by its own thread, never
    /// by the loop thread.
    PiProbed {
        /// The probe, as numbered when its ask was parked.
        id: u64,
        /// Whether it is a tested version, or the refusal text.
        verdict: Result<(), String>,
    },
    /// Run a command and answer it.
    Ask {
        /// What to do.
        ask: Ask,
        /// Where the answer goes.
        reply: Sender<Result<Answered, RpcError>>,
    },
    /// Deliver one normalized hook event and optionally retain its reply path.
    Hook {
        /// Correlates the supervisor's reply with this caller.
        hook: HookId,
        /// The structurally attributed event.
        event: Event,
        /// Absent for the three Claude events with no decision control.
        reply: Option<Sender<Result<HookAnswer, RpcError>>>,
        /// The hook-specific admission slot, held until this job is pumped or dropped.
        permit: crate::hooks::HookPermit,
    },
    /// Read `system.doctor`'s loop-owned adoption-key and replay-derived diagnostics.
    Doctor {
        /// Where the diagnostic snapshot goes.
        reply: Sender<Result<DoctorSnapshot, RpcError>>,
    },
    /// Take the one subscription to an attached agent's exact PTY output.
    PtyOutput {
        /// The attached agent.
        agent: AgentId,
        /// The handle the caller was given for that attachment.
        pty: PtyId,
        /// Where the subscription goes.
        reply: Sender<Result<OutputSubscription, RpcError>>,
    },
    /// A connection or a browser terminal dropped its exact attached capability.
    AttachmentLost {
        /// The attached agent.
        agent: AgentId,
        /// The exact handle that connection owned.
        pty: PtyId,
        /// Why, as the system close request records it.
        reason: &'static str,
    },
    /// Stop, for this reason.
    Stop(ExitCause),
    /// One authenticated display rename admitted under Floor control.
    Rename {
        /// Registry-derived session.
        session: crate::floor_session::SessionIdentity,
        /// Canonical request fields.
        request: crate::floor_rename::RenameRequest,
        /// Durable original result or no-effect refusal.
        reply:
            Sender<Result<crate::floor_rename::RenameOutcome, crate::floor_rename::RenameRefusal>>,
    },
    /// One authenticated appearance edit admitted under Floor control.
    Look {
        /// Registry-derived session.
        session: crate::floor_session::SessionIdentity,
        /// Canonical request fields.
        request: crate::floor_look::LookRequest,
        /// Durable original result or no-effect refusal.
        reply: Sender<Result<crate::floor_look::LookOutcome, crate::floor_look::LookRefusal>>,
    },
    /// One typed browser-note operation.
    Chat {
        /// The admitting registry session.
        session: crate::floor_session::SessionIdentity,
        /// Canonical boundary fields.
        request: crate::floor_chat::ChatRequest,
        /// The durable receipt or fixed refusal.
        reply: Sender<Result<crate::floor_chat::ChatOutcome, crate::floor_chat::ChatRefusal>>,
    },
}

/// One private closing call's operation and optional writer-thread reply.
#[derive(Debug)]
pub struct ClosingJob {
    pub(crate) request: crate::closing::Request,
    pub(crate) reply: Option<Sender<Result<crate::closing::Answer, RpcError>>>,
}

/// How every other thread reaches the loop.
///
/// `Send + Sync` and cheap to clone, because accept-loop handlers, the reaper and every predicate
/// thread hold one too.
#[derive(Clone, Debug)]
pub struct Handle {
    jobs: Sender<Job>,
    floor_admission: Arc<Mutex<()>>,
    #[cfg(test)]
    floor_submission_pause: Arc<Mutex<Option<FloorSubmissionPause>>>,
    next_hook: Arc<AtomicU64>,
    journal_uncertain: Arc<AtomicBool>,
    #[cfg(feature = "testing")]
    closing_calls: Arc<AtomicU64>,
    #[cfg(feature = "testing")]
    predicate_completions: Arc<AtomicU64>,
    journal_stopped: Arc<Mutex<Option<Lsn>>>,
    /// Set once the exit path starts. A thread waiting on a goal's check run, or on the append
    /// after it, stops waiting, so it never holds the exit path's join on connections.
    stopping: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unavailable;

#[cfg(test)]
#[derive(Debug)]
struct FloorSubmissionPause {
    entered: Sender<()>,
    release: Receiver<()>,
}

impl Handle {
    fn send_floor(&self, job: Job) -> Result<(), Unavailable> {
        let _admission = self
            .floor_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.stopping.load(Ordering::Acquire) {
            return Err(Unavailable);
        }
        #[cfg(test)]
        if let Some(pause) = self
            .floor_submission_pause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            pause.entered.send(()).expect("Floor admission reached");
            pause
                .release
                .recv_timeout(Duration::from_secs(30))
                .expect("the test releases Floor admission");
        }
        self.jobs.send(job).map_err(|_| Unavailable)
    }

    /// Queue one strictly typed Chat operation on the daemon writer.
    ///
    /// # Errors
    ///
    /// A stopped writer, invalid session, conflicting durable retry, or uncertain append.
    pub fn chat(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_chat::ChatRequest,
    ) -> Result<crate::floor_chat::ChatOutcome, crate::floor_chat::ChatRefusal> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send_floor(Job::Chat {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_chat::ChatRefusal::UnavailableNoEffect)?;
        answer
            .recv()
            .map_err(|_| crate::floor_chat::ChatRefusal::Unavailable)?
    }
    pub(crate) fn onboard_continue(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_actions::OnboardRequest,
        reply: Sender<crate::floor_actions::OnboardStep>,
    ) {
        let _ = self.send_floor(Job::OnboardContinue {
            session,
            request,
            reply,
        });
    }
    /// A browser hire end to end: its admission, its check's run once before anything is
    /// appended, then the admission again and the hire. The owner of the request waits for the
    /// run; a same-key follower is answered pending by the session's reservation meanwhile.
    pub(crate) fn onboard(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_actions::OnboardRequest,
    ) -> Result<crate::floor_actions::OnboardOutcome, crate::floor_actions::OnboardRefusal> {
        use crate::floor_actions::{OnboardRefusal, OnboardStep};
        let step = self.onboard_step(|reply| Job::Onboard {
            session: session.clone(),
            request: request.clone(),
            reply,
        })?;
        let (ticket, on_refusal) = match step {
            OnboardStep::Answer(answer) => return answer,
            OnboardStep::Check { ticket, on_refusal } => (ticket, on_refusal),
        };
        let no_effect = if on_refusal.is_some() {
            OnboardRefusal::Unavailable
        } else {
            OnboardRefusal::UnavailableNoEffect
        };
        let never = dudes_rpc::HangUp::never();
        match crate::baseline::admit_after_baseline(&ticket, &self.stopping, &never, |baseline| {
            self.onboard_step(|reply| Job::OnboardChecked {
                session,
                request,
                baseline,
                run: ticket.id,
                reply,
            })
        }) {
            Ok(Ok(OnboardStep::Answer(answer))) => answer,
            Err(crate::baseline::Baseline::Refused(refusal)) => {
                on_refusal.ok_or(OnboardRefusal::Check(refusal.reason()))
            }
            Ok(Ok(OnboardStep::Check { .. }) | Err(_)) | Err(_) => Err(no_effect),
        }
    }

    /// Send one onboarding job and wait for its step, answering unavailable once the daemon is
    /// stopping.
    fn onboard_step(
        &self,
        job: impl FnOnce(Sender<crate::floor_actions::OnboardStep>) -> Job,
    ) -> Result<crate::floor_actions::OnboardStep, crate::floor_actions::OnboardRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(job(reply))
            .map_err(|_| crate::floor_actions::OnboardRefusal::Unavailable)?;
        crate::baseline::recv_until_stopping(&self.stopping, &answers)
            .ok_or(crate::floor_actions::OnboardRefusal::Unavailable)
    }

    pub(crate) fn answer(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_answer::AnswerRequest,
    ) -> Result<crate::floor_answer::AnswerOutcome, crate::floor_answer::AnswerRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::Answer {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_answer::AnswerRefusal::UnavailableNoEffect)?;
        answers
            .recv()
            .map_err(|_| crate::floor_answer::AnswerRefusal::Unavailable)?
    }
    pub(crate) fn control(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_control::ControlRequest,
    ) -> Result<crate::floor_control::ControlOutcome, crate::floor_control::ControlRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::Control {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_control::ControlRefusal::UnavailableNoEffect)?;
        answers
            .recv()
            .map_err(|_| crate::floor_control::ControlRefusal::Unavailable)?
    }
    pub(crate) fn goal(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_goal::GoalRequest,
    ) -> Result<crate::floor_goal::GoalOutcome, crate::floor_goal::GoalRefusal> {
        use crate::floor_goal::{GoalOutcome, GoalRefusal, GoalStep};
        let step = |job: Job, answers: Receiver<GoalStep>| -> Result<GoalStep, GoalRefusal> {
            self.send_floor(job)
                .map_err(|_| GoalRefusal::UnavailableNoEffect)?;
            crate::baseline::recv_until_stopping(&self.stopping, &answers)
                .ok_or(GoalRefusal::Unavailable)
        };
        let (reply, answers) = mpsc::channel();
        let ticket = match step(
            Job::Goal {
                session: session.clone(),
                request: request.clone(),
                reply,
            },
            answers,
        )? {
            GoalStep::Answer(answer) => return answer,
            GoalStep::Check(ticket) => ticket,
        };
        let request_id = request.request_wire.clone();
        let never = dudes_rpc::HangUp::never();
        match crate::baseline::admit_after_baseline(&ticket, &self.stopping, &never, |baseline| {
            let (reply, answers) = mpsc::channel();
            step(
                Job::GoalChecked {
                    session,
                    request,
                    baseline,
                    run: ticket.id,
                    reply,
                },
                answers,
            )
        }) {
            Ok(Ok(GoalStep::Answer(answer))) => answer,
            Ok(Err(refusal)) => Err(refusal),
            Err(crate::baseline::Baseline::Refused(refusal)) => Ok(GoalOutcome::Refused {
                request_id,
                reason: refusal.reason(),
            }),
            Ok(Ok(GoalStep::Check(_))) | Err(_) => Err(GoalRefusal::UnavailableNoEffect),
        }
    }
    pub(crate) fn rename(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_rename::RenameRequest,
    ) -> Result<crate::floor_rename::RenameOutcome, crate::floor_rename::RenameRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::Rename {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_rename::RenameRefusal::UnavailableNoEffect)?;
        answers
            .recv()
            .map_err(|_| crate::floor_rename::RenameRefusal::Unavailable)?
    }
    pub(crate) fn look(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_look::LookRequest,
    ) -> Result<crate::floor_look::LookOutcome, crate::floor_look::LookRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::Look {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_look::LookRefusal::UnavailableNoEffect)?;
        answers
            .recv()
            .map_err(|_| crate::floor_look::LookRefusal::Unavailable)?
    }
    /// What the journal records about `agent`'s tree, or `None` for an unknown agent.
    pub(crate) fn git_target(
        &self,
        agent: AgentId,
    ) -> Result<Option<crate::floor_git::GitTarget>, Unavailable> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::GitTarget { agent, reply })?;
        answers.recv().map_err(|_| Unavailable)
    }
    /// The loop's answer to one Floor memory read, or `None` when the loop has ended or is ending.
    pub(crate) fn floor_memory(
        &self,
        request: crate::floor_memory::MemoryRequest,
    ) -> Option<crate::floor_memory::MemoryAnswer> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::FloorMemory { request, reply }).ok()?;
        answers.recv().ok()
    }
    pub(crate) fn retire(
        &self,
        session: crate::floor_session::SessionIdentity,
        request: crate::floor_retire::RetireRequest,
    ) -> Result<crate::floor_retire::RetireOutcome, crate::floor_retire::RetireRefusal> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::Retire {
            session,
            request,
            reply,
        })
        .map_err(|_| crate::floor_retire::RetireRefusal::UnavailableNoEffect)?;
        answers
            .recv()
            .map_err(|_| crate::floor_retire::RetireRefusal::Unavailable)?
    }
    pub(crate) fn floor_subscribe(
        &self,
        session: crate::floor_session::SessionIdentity,
    ) -> Result<crate::floor_stream::Opened, String> {
        let (reply, answers) = mpsc::channel();
        self.send_floor(Job::FloorSubscribe { session, reply })
            .map_err(|_| "the daemon loop has ended".to_owned())?;
        answers
            .recv()
            .map_err(|_| "the daemon loop has ended".to_owned())?
    }
    pub(crate) fn enrollment_reload(
        &self,
        read: crate::enrollment_reload::ReloadRead,
    ) -> Result<dudes_proto::rpc::floor::EnrollmentReloadResult, RpcError> {
        let (reply, answers) = mpsc::channel();
        self.jobs
            .send(Job::EnrollmentReload {
                read: Box::new(read),
                reply,
            })
            .map_err(|_| unavailable())?;
        answers.recv().map_err(|_| unavailable())?
    }

    pub(crate) fn enrollment_resync(&self, window_id: u64) {
        let _ = self.jobs.send(Job::EnrollmentResync { window_id });
    }

    /// Wrap the loop's sender.
    #[must_use]
    pub fn new(jobs: Sender<Job>) -> Self {
        Self {
            jobs,
            floor_admission: Arc::new(Mutex::new(())),
            #[cfg(test)]
            floor_submission_pause: Arc::new(Mutex::new(None)),
            next_hook: Arc::new(AtomicU64::new(1)),
            journal_uncertain: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "testing")]
            closing_calls: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "testing")]
            predicate_completions: Arc::new(AtomicU64::new(0)),
            journal_stopped: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Mark the daemon as stopping, before the exit path joins connections.
    pub(crate) fn stop_waiting(&self) {
        let _admission = self
            .floor_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.stopping.store(true, Ordering::Release);
    }

    /// One goal's check run before the first turn ended, however it ended.
    pub(crate) fn baseline_ended(&self, id: u64) {
        let _ = self.jobs.send(Job::BaselineEnded { id });
    }

    /// `goal.set` end to end: its admission, its check's run once before the first turn when the
    /// check must run, then the admission again and the append. A caller that `hang_up` reports
    /// gone gets no goal.
    ///
    /// # Errors
    ///
    /// A refusal from either admission or from the run, or unavailable when the run reached no
    /// verdict or the daemon is stopping.
    pub fn set_goal(
        &self,
        params: GoalSetParams,
        hang_up: &dudes_rpc::HangUp,
    ) -> Result<GoalSetResult, RpcError> {
        let ticket = match self.ask(Ask::SetGoal(Box::new(params.clone())))? {
            Answered::GoalSet(result) => return Ok(result),
            Answered::CheckRunning(ticket) => ticket,
            other => {
                return Err(internal(format!(
                    "the loop answered {other:?} for goal.set"
                )));
            }
        };
        let appended =
            crate::baseline::admit_after_baseline(&ticket, &self.stopping, hang_up, |baseline| {
                if hang_up.hung_up() {
                    return Err(caller_gone());
                }
                self.ask_until_stopping(Ask::SetGoalChecked {
                    params: Box::new(params),
                    baseline,
                    run: ticket.id,
                    hang_up: hang_up.clone(),
                })
            });
        match appended {
            Ok(Ok(Answered::GoalSet(result))) => Ok(result),
            Ok(Ok(other)) => Err(internal(format!(
                "the loop answered {other:?} for goal.set"
            ))),
            Ok(Err(error)) => Err(error),
            Err(crate::baseline::Baseline::Refused(refusal)) => Err(
                crate::baseline::goal_set_refusal(&refusal, &ticket.passed(), ticket.timeout_s),
            ),
            Err(_) if hang_up.hung_up() => Err(caller_gone()),
            Err(_) => Err(unavailable()),
        }
    }

    /// `goal.replace` after [`Answered::CheckRunning`]: the run, then steps 2 to 5 again and the
    /// append. A replacement keeps running after its caller hangs up, because its request
    /// identity answers the retry.
    fn replace_after_baseline(
        &self,
        params: dudes_proto::rpc::goal::GoalReplaceParams,
        ticket: &crate::baseline::Ticket,
    ) -> Result<dudes_proto::rpc::goal::GoalReplaceResult, RpcError> {
        use dudes_proto::rpc::goal::{GoalReplaceRefusal, GoalReplaceResult};
        let request_id = params.request_id;
        let refused = |reason| Ok(GoalReplaceResult::Refused { request_id, reason });
        match crate::baseline::admit_after_baseline(
            ticket,
            &self.stopping,
            &dudes_rpc::HangUp::never(),
            |baseline| {
                self.ask_until_stopping(Ask::ReplaceGoalChecked {
                    params: Box::new(params),
                    baseline,
                    run: ticket.id,
                })
            },
        ) {
            Ok(Ok(Answered::GoalReplaced(result))) => Ok(result),
            Ok(Ok(_)) => Err(internal("unexpected goal.replace answer")),
            Ok(Err(error)) if error.code == ErrorCode::InternalError.code() => {
                refused(GoalReplaceRefusal::Unavailable)
            }
            Ok(Err(error)) => Err(error),
            Err(crate::baseline::Baseline::Refused(refusal)) => refused(match refusal {
                crate::baseline::Refusal::CannotRun(_) => GoalReplaceRefusal::CheckCannotRun,
                crate::baseline::Refusal::AlreadyPasses => GoalReplaceRefusal::CheckAlreadyPasses,
                crate::baseline::Refusal::DidNotFinish => GoalReplaceRefusal::CheckDidNotFinish,
            }),
            Err(_) => refused(GoalReplaceRefusal::Unavailable),
        }
    }

    /// [`Self::ask`] for the append after a goal's check run: a daemon that starts stopping while
    /// the job waits answers unavailable at once, and the job, when the exit path drains it,
    /// appends nothing.
    ///
    /// # Errors
    ///
    /// As [`Self::ask`], and unavailable once the daemon is stopping.
    pub(crate) fn ask_until_stopping(&self, ask: Ask) -> Result<Answered, RpcError> {
        let answers = self.ask_queued(ask)?;
        crate::baseline::recv_until_stopping(&self.stopping, &answers)
            .unwrap_or_else(|| Err(unavailable()))
    }

    pub(crate) fn journal_uncertain(&self) -> bool {
        self.journal_uncertain.load(Ordering::Acquire)
    }

    pub(crate) fn fence_journal(&self) -> bool {
        !self.journal_uncertain.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn journal_denial() -> HookReply {
        HookReply {
            decision: dudes_proto::HookDecision::Deny {
                reason: "journal state unresolved; restart to replay before new work or writes"
                    .into(),
            },
            context: None,
        }
    }

    pub(crate) fn publish_journal_stopped(&self, lsn: Lsn) {
        *self
            .journal_stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(lsn);
    }

    pub(crate) fn require_journal_writes(&self) -> Result<(), RpcError> {
        if let Some(lsn) = *self
            .journal_stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(journal_stopped_error(lsn));
        }
        if self.journal_uncertain() {
            return Err(dudes_rpc::dispatch::error(
                ErrorCode::Refused,
                "journal state unresolved; restart to replay before new work or writes",
            ));
        }
        Ok(())
    }

    pub(crate) fn protocol_failed(&self, key: dudes_supervisor::host::AdoptionKey) {
        let _ = self.jobs.send(Job::ProtocolFailed(key));
    }

    /// Post an input. Silently dropped once the loop has stopped, which is the right behaviour for
    /// a reaper tick or a late child exit: the exit sequence is already running and nothing it
    /// produced would be journaled anyway.
    pub fn input(&self, input: dudes_supervisor::Input) {
        let _ = self.jobs.send(Job::Input(Box::new(input)));
    }

    /// Report that one predicate run ended. Silently dropped once the loop has stopped, as
    /// [`Self::input`] is.
    pub(crate) fn predicate_ended(&self, goal: GoalId, verdict: Option<(bool, Evidence)>) {
        let sent = self.jobs.send(Job::PredicateEnded { goal, verdict });
        #[cfg(feature = "testing")]
        if sent.is_ok() {
            self.predicate_completions.fetch_add(1, Ordering::Release);
        }
        #[cfg(not(feature = "testing"))]
        let _ = sent;
    }

    pub(crate) fn pi_probed(&self, id: u64, verdict: Result<(), String>) {
        let _ = self.jobs.send(Job::PiProbed { id, verdict });
    }

    pub(crate) fn memory_condense_model_ended(
        &self,
        agent: AgentId,
        plan: dudes_journal::CondensePlan,
        outcome: Option<crate::predicate::PredicateCaptured>,
    ) {
        let _ = self.jobs.send(Job::MemoryCondenseModelEnded {
            agent,
            plan,
            outcome,
        });
    }

    /// Ask the daemon to stop.
    pub fn stop(&self, cause: ExitCause) {
        let _ = self.jobs.send(Job::Stop(cause));
    }

    /// Subscribe to the output of `agent`'s exact live PTY.
    ///
    /// # Errors
    ///
    /// Refused when `pty` is not the agent's live attachment, its output was already subscribed,
    /// or the daemon is shutting down; [`ErrorCode::InternalError`] once the loop has stopped.
    pub fn subscribe_output(
        &self,
        agent: AgentId,
        pty: PtyId,
    ) -> Result<OutputSubscription, RpcError> {
        let (reply, answers) = mpsc::channel();
        self.jobs
            .send(Job::PtyOutput { agent, pty, reply })
            .map_err(|_| unavailable())?;
        answers.recv().map_err(|_| unavailable())?
    }

    /// Queue connection-loss cleanup without waiting on the loop this destructor may unblock.
    pub fn attachment_lost(&self, agent: AgentId, pty: PtyId) {
        self.close_attachment(agent, pty, "connection lost");
    }

    pub(crate) fn close_attachment(&self, agent: AgentId, pty: PtyId, reason: &'static str) {
        let _ = self.jobs.send(Job::AttachmentLost { agent, pty, reason });
    }

    /// Run a command and wait for its answer.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has already stopped, which is what an operator
    /// calling during shutdown sees.
    pub fn ask(&self, ask: Ask) -> Result<Answered, RpcError> {
        self.ask_queued(ask)?.recv().map_err(|_| unavailable())?
    }

    pub(crate) fn closing_call(
        &self,
        request: crate::closing::Request,
    ) -> Result<crate::closing::Answer, RpcError> {
        let (reply, answers) = mpsc::channel();
        self.jobs
            .send(Job::Closing(Box::new(ClosingJob {
                request,
                reply: Some(reply),
            })))
            .map_err(|_| unavailable())?;
        #[cfg(feature = "testing")]
        self.closing_calls.fetch_add(1, Ordering::Relaxed);
        answers.recv().map_err(|_| unavailable())?
    }

    pub(crate) fn closing_after_response(&self) {
        let _ = self.jobs.send(Job::Closing(Box::new(ClosingJob {
            request: crate::closing::Request::AfterResponse,
            reply: None,
        })));
    }

    /// Queue one typed call and return its reply channel for deterministic writer-order tests.
    #[doc(hidden)]
    pub fn ask_queued(&self, ask: Ask) -> Result<Receiver<Result<Answered, RpcError>>, RpcError> {
        let (reply, answers) = mpsc::channel();
        if matches!(&ask, Ask::BrowserTerminal(_)) {
            self.send_floor(Job::Ask { ask, reply })
                .map_err(|_| unavailable())?;
        } else {
            self.jobs
                .send(Job::Ask { ask, reply })
                .map_err(|_| unavailable())?;
        }
        Ok(answers)
    }

    /// Predicate completions queued for the writer, including completions without a verdict.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn predicate_completions_queued(&self) -> u64 {
        self.predicate_completions.load(Ordering::Acquire)
    }

    /// How many hook events this handle and its clones have queued for the loop. A hook is queued
    /// only after the seam authorised it, so a test holding the loop can release it on this count.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn hooks_queued(&self) -> u64 {
        self.next_hook.load(Ordering::Relaxed) - 1
    }

    /// Calls admitted by the closing router and queued for current writer-thread checks.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn closing_calls_queued(&self) -> u64 {
        self.closing_calls.load(Ordering::Relaxed)
    }

    /// Queue one isolated hook and return its actual writer reply for ordering tests.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has already stopped.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn hook_queued_for_test(
        &self,
        event: Event,
    ) -> Result<Receiver<Result<HookAnswer, RpcError>>, RpcError> {
        let permit = crate::hooks::HookPermit::isolated_for_test(event.agent);
        let (reply, answers) = mpsc::channel();
        self.queue_hook(event, Some(reply), permit)?;
        Ok(answers)
    }

    /// Queue a hook event whose vendor cannot act on a reply.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has already stopped.
    pub fn observe_hook(
        &self,
        event: Event,
        permit: crate::hooks::HookPermit,
    ) -> Result<(), RpcError> {
        self.queue_hook(event, None, permit)
    }

    /// Queue a hook event and wait at most two seconds for the supervisor's reply.
    ///
    /// A control deadline blocks that step. Other event deadlines fail open unless journal
    /// coherence is uncertain. Every deadline queues the ordinary `HookTimeout` signal.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has stopped before taking or answering the event.
    pub fn hook(
        &self,
        event: Event,
        permit: crate::hooks::HookPermit,
    ) -> Result<HookReply, RpcError> {
        const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

        let agent = event.agent;
        let control = matches!(
            &event.kind,
            dudes_proto::EventKind::PreTool { .. } | dudes_proto::EventKind::PromptSubmit
        );
        let (reply, answer) = mpsc::channel();
        let lease = permit.reply_lease();
        self.queue_hook(event, Some(reply), permit)?;
        let (answered, preserve_delivery_error) = match answer.recv_timeout(REPLY_TIMEOUT) {
            Ok(reply) => (reply, false),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                lease.expire();
                tracing::error!(?agent, "the supervisor timed out answering a hook");
                self.input(dudes_supervisor::Input::Signal {
                    agent,
                    signal: RuntimeSignal::HookTimeout,
                });
                let reply = if control {
                    HookReply {
                        decision: dudes_proto::HookDecision::Deny {
                            reason: "the daemon did not answer the control hook; retry, and restart duded if this persists".to_owned(),
                        },
                        context: None,
                    }
                } else {
                    HookReply::ALLOW
                };
                (Ok(HookAnswer::from(reply)), !control)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => (Err(unavailable()), true),
        };
        let stopped = self
            .journal_stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let answered = if let Some(lsn) = *stopped {
            if preserve_delivery_error {
                Err(journal_stopped_error(lsn))
            } else {
                match answered {
                    Ok(answer) if answer.stopped_at == Some(lsn) => Ok(answer.reply),
                    Ok(answer)
                        if answer.reply.context.is_none()
                            && matches!(
                                answer.reply.decision,
                                dudes_proto::HookDecision::Deny { .. }
                                    | dudes_proto::HookDecision::Halt
                            ) =>
                    {
                        Ok(answer.reply)
                    }
                    Err(error) => Err(error),
                    _ => Err(journal_stopped_error(lsn)),
                }
            }
        } else {
            answered.map(|answer| answer.reply)
        };
        if self.journal_uncertain() {
            Ok(
                if matches!(
                    &answered,
                    Ok(HookReply {
                        decision: dudes_proto::HookDecision::Halt,
                        ..
                    })
                ) {
                    HookReply {
                        decision: dudes_proto::HookDecision::Halt,
                        context: None,
                    }
                } else {
                    Self::journal_denial()
                },
            )
        } else {
            answered
        }
    }

    fn queue_hook(
        &self,
        event: Event,
        reply: Option<Sender<Result<HookAnswer, RpcError>>>,
        permit: crate::hooks::HookPermit,
    ) -> Result<(), RpcError> {
        if reply.is_none() {
            self.require_journal_writes()?;
        }
        let hook = HookId(self.next_hook.fetch_add(1, Ordering::Relaxed));
        self.jobs
            .send(Job::Hook {
                hook,
                event,
                reply,
                permit,
            })
            .map_err(|_| unavailable())
    }

    /// Read `system.doctor`'s loop-owned adoption-key and replay-derived diagnostics.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has already stopped.
    pub fn doctor(&self) -> Result<DoctorSnapshot, RpcError> {
        self.queue_doctor()?.recv().map_err(|_| unavailable())?
    }

    /// Queue a diagnostic read and return its reply separately.
    ///
    /// This is the two-phase form of [`Self::doctor`]. It lets shutdown tests establish queue order
    /// before the loop starts and put a bound on the wait, rather than relying on thread timing.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InternalError`] when the loop has already stopped.
    #[doc(hidden)]
    pub fn queue_doctor(&self) -> Result<Receiver<Result<DoctorSnapshot, RpcError>>, RpcError> {
        let (reply, answers) = mpsc::channel();
        self.jobs
            .send(Job::Doctor { reply })
            .map_err(|_| unavailable())?;
        Ok(answers)
    }
}

/// The refusal a caller gets when the loop is gone.
fn unavailable() -> RpcError {
    internal("the daemon is shutting down")
}

/// The answer to a `goal.set` whose caller hung up before its append: nothing was journaled.
fn caller_gone() -> RpcError {
    error(
        ErrorCode::Refused,
        "goal.set: the caller hung up before its check finished, so no goal was set",
    )
}

/// An error with a message, for the ports below.
fn internal(message: impl Into<String>) -> RpcError {
    error(ErrorCode::InternalError, message)
}

/// `Control`, over the loop.
#[derive(Clone, Debug)]
pub struct LoopControl {
    handle: Handle,
    operator_authority: Arc<dudes_rpc::OperatorAuthoritySource>,
    replacements: Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<dudes_proto::record::Hex32, dudes_proto::record::Hex32>,
        >,
    >,
    worktrees: Option<Arc<crate::worktree::hire::WorktreeHires>>,
}

impl LoopControl {
    /// A control port over `handle`.
    #[must_use]
    pub fn new(
        handle: Handle,
        operator_authority: Arc<dudes_rpc::OperatorAuthoritySource>,
    ) -> Self {
        Self {
            handle,
            operator_authority,
            replacements: Arc::default(),
            worktrees: None,
        }
    }

    /// This port, able to hire into a worktree through `worktrees`.
    #[must_use]
    pub fn with_worktrees(mut self, worktrees: Arc<crate::worktree::hire::WorktreeHires>) -> Self {
        self.worktrees = Some(worktrees);
        self
    }

    /// Ask, and require the answer to be `Done`.
    fn done(&self, ask: Ask) -> Result<(), RpcError> {
        match self.handle.ask(ask)? {
            Answered::Done => Ok(()),
            other => Err(internal(format!(
                "the loop answered {other:?} for a command that \
                                           reports nothing"
            ))),
        }
    }
}

impl Control for LoopControl {
    fn hire(&self, params: HireParams) -> Result<HireResult, RpcError> {
        // Seal before the job can reach the loop. A racing accept linearizes on the source's same
        // lock and therefore either acquired in the pre-agent epoch or observes this seal. No
        // validation, persistence, provider, or spawn failure below can reopen it.
        self.operator_authority.seal();
        if params.checkout.is_some() {
            return match &self.worktrees {
                Some(worktrees) => worktrees.hire(params),
                None => Err(crate::worktree::unavailable(
                    "this daemon was started without worktree support",
                )),
            };
        }
        match self.handle.ask(Ask::Hire(Box::new(params)))? {
            Answered::Hired(result) => Ok(result),
            other => Err(internal(format!(
                "the loop answered {other:?} for agent.hire"
            ))),
        }
    }

    fn worktree_list(&self) -> Result<Vec<dudes_proto::rpc::worktree::WorktreeEntry>, RpcError> {
        match self.handle.ask(Ask::WorktreeEntries)? {
            Answered::WorktreeEntries(entries) => Ok(entries),
            other => Err(internal(format!(
                "the loop answered {other:?} for worktree.list"
            ))),
        }
    }

    fn worktree_removed(
        &self,
        params: dudes_proto::rpc::worktree::RemovedParams,
    ) -> Result<(), RpcError> {
        self.done(Ask::WorktreeRemoved(params.worktree))
    }

    fn rename(&self, params: RenameParams) -> Result<(), RpcError> {
        self.done(Ask::Rename(Box::new(params)))
    }

    fn look(&self, params: LookParams) -> Result<(), RpcError> {
        self.done(Ask::Look(Box::new(params)))
    }

    fn mcp(&self, params: McpParams) -> Result<McpResult, RpcError> {
        match self.handle.ask(Ask::Mcp(params))? {
            Answered::Mcp(result) => Ok(result),
            other => Err(internal(format!(
                "the loop answered {other:?} for agent.mcp"
            ))),
        }
    }

    fn retire(&self, params: RetireParams) -> Result<(), RpcError> {
        self.done(Ask::Retire(Box::new(params)))
    }

    fn set_goal(
        &self,
        params: GoalSetParams,
        hang_up: &dudes_rpc::HangUp,
    ) -> Result<GoalSetResult, RpcError> {
        self.handle.set_goal(params, hang_up)
    }

    fn replace_goal(
        &self,
        params: dudes_proto::rpc::goal::GoalReplaceParams,
    ) -> Result<dudes_proto::rpc::goal::GoalReplaceResult, RpcError> {
        use dudes_proto::rpc::goal::{GoalReplaceRefusal, GoalReplaceResult};
        params.validate().map_err(|reason| {
            dudes_rpc::dispatch::error(dudes_proto::rpc::ErrorCode::InvalidParams, reason)
        })?;
        let request_id = params.request_id;
        let check = crate::goal_replacement::check_reference(&params);
        let digest = crate::goal_replacement::request_digest(&params, &check);
        {
            let mut pending = self
                .replacements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prior) = pending.get(&request_id) {
                return Ok(if *prior == digest {
                    GoalReplaceResult::Pending { request_id }
                } else {
                    GoalReplaceResult::Refused {
                        request_id,
                        reason: GoalReplaceRefusal::RequestConflict,
                    }
                });
            }
            pending.insert(request_id, digest);
        }
        let result = match self.handle.ask(Ask::ReplaceGoal(Box::new(params.clone()))) {
            Ok(Answered::GoalReplaced(result)) => Ok(result),
            Ok(Answered::CheckRunning(ticket)) => {
                self.handle.replace_after_baseline(params, &ticket)
            }
            Ok(_) => Err(internal("unexpected goal.replace answer")),
            Err(error) if error.code == dudes_proto::rpc::ErrorCode::InternalError.code() => {
                Ok(GoalReplaceResult::Pending { request_id })
            }
            Err(error) => Err(error),
        };
        if !matches!(result, Ok(GoalReplaceResult::Pending { .. })) {
            self.replacements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request_id);
        }
        result
    }

    fn pause(&self, agent: AgentId) -> Result<(), RpcError> {
        self.done(Ask::Pause(agent))
    }

    fn resume(&self, agent: AgentId) -> Result<(), RpcError> {
        self.done(Ask::Resume(agent))
    }

    fn halt(&self, agent: AgentId) -> Result<(), RpcError> {
        self.done(Ask::Halt(agent))
    }

    fn ask(&self, agent: AgentId, text: String) -> Result<(), RpcError> {
        self.done(Ask::Ask { agent, text })
    }
}

impl LoopControl {
    /// Convert one conditioned persistence operation to the narrow loop command that can make it
    /// durable. A generic `Record` command would be a second writer surface (invariant 1).
    fn conditioned(
        &self,
        method: ConditionedMethod,
        operation: &PersistenceOperation<'_, AgentText>,
    ) -> Result<(), RpcError> {
        if let Some(record) = operation.disposition_record() {
            let Record::ConditionedEffectSuppressed {
                method,
                target,
                provenance,
                reason,
                payload,
            } = record
            else {
                return Err(internal(
                    "conditioned agent text produced a non-suppression record",
                ));
            };
            return self.done(Ask::ConditionedEffectSuppressed {
                method: *method,
                target: target.clone(),
                provenance: *provenance,
                reason: reason.clone(),
                payload: payload.clone(),
            });
        }

        let applied = operation
            .applied_effect()
            .ok_or_else(|| internal("conditioned persistence operation had no outcome"))?;
        let provenance = applied.provenance();
        let params = applied.params().clone();
        debug_assert_eq!(
            applied.target(),
            &ConditionedTarget::Agent {
                agent: params.agent
            }
        );
        match method {
            ConditionedMethod::AgentAnswer => self.done(Ask::Answer {
                agent: params.agent,
                text: params.text,
                provenance,
            }),
            ConditionedMethod::AgentSteer => self.done(Ask::Steer {
                agent: params.agent,
                text: params.text,
                provenance,
            }),
            _ => Err(internal(
                "non-agent-text method reached the agent text port",
            )),
        }
    }
}

impl ConditionedAgentControl for LoopControl {
    fn answer(&self, operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
        self.conditioned(ConditionedMethod::AgentAnswer, &operation)
    }

    fn steer(&self, operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
        self.conditioned(ConditionedMethod::AgentSteer, &operation)
    }
}

impl ConditionedAttachControl for LoopControl {
    fn attach(&self, operation: PersistenceOperation<'_, AttachParams>) -> Result<PtyId, RpcError> {
        if let Some(record) = operation.disposition_record() {
            let Record::ConditionedEffectSuppressed {
                method,
                target,
                provenance,
                reason,
                payload,
            } = record
            else {
                return Err(internal(
                    "conditioned attach produced a non-suppression record",
                ));
            };
            self.done(Ask::ConditionedEffectSuppressed {
                method: *method,
                target: target.clone(),
                provenance: *provenance,
                reason: reason.clone(),
                payload: payload.clone(),
            })?;
            return Ok(PtyId::default());
        }

        let applied = operation
            .applied_effect()
            .ok_or_else(|| internal("conditioned attach persistence had no outcome"))?;
        let params = *applied.params();
        debug_assert_eq!(
            applied.target(),
            &ConditionedTarget::Agent {
                agent: params.agent
            }
        );
        match self.handle.ask(Ask::Attach {
            agent: params.agent,
            by: applied.actor(),
            provenance: applied.provenance(),
            cols: params.cols,
            rows: params.rows,
        })? {
            Answered::Attached(pty) => Ok(pty),
            other => Err(internal(format!(
                "the loop answered {other:?} for agent.attach"
            ))),
        }
    }

    fn connection_lost(&self, agent: AgentId, pty: PtyId) {
        self.handle.attachment_lost(agent, pty);
    }
}

impl ConditionedDetachControl for LoopControl {
    fn detach(
        &self,
        operation: PersistenceOperation<'_, AgentRef>,
        pty: Option<PtyId>,
    ) -> Result<Option<dudes_proto::PtyCloseOutcome>, RpcError> {
        if let Some(record) = operation.disposition_record() {
            let Record::ConditionedEffectSuppressed {
                method,
                target,
                provenance,
                reason,
                payload,
            } = record
            else {
                return Err(internal(
                    "conditioned detach produced a non-suppression record",
                ));
            };
            self.done(Ask::ConditionedEffectSuppressed {
                method: *method,
                target: target.clone(),
                provenance: *provenance,
                reason: reason.clone(),
                payload: payload.clone(),
            })?;
            return Ok(None);
        }
        let applied = operation
            .applied_effect()
            .ok_or_else(|| internal("conditioned detach persistence had no outcome"))?;
        let pty = pty.ok_or_else(|| internal("agent.detach had no bound PTY"))?;
        let agent = applied.params().agent;
        debug_assert_eq!(applied.target(), &ConditionedTarget::Agent { agent });
        match self.handle.ask(Ask::Detach {
            agent,
            pty,
            by: applied.actor(),
            provenance: applied.provenance(),
        })? {
            Answered::PtyClosed(outcome) => Ok(Some(outcome)),
            other => Err(internal(format!(
                "the loop answered {other:?} for agent.detach"
            ))),
        }
    }
}

impl ConditionedPtyControl for LoopControl {
    fn write(
        &self,
        operation: PersistenceOperation<'_, PtyWriteParams>,
    ) -> Result<PtyWriteOutcome, RpcError> {
        if let Some(record) = operation.disposition_record() {
            let Record::ConditionedEffectSuppressed {
                method,
                target,
                provenance,
                reason,
                payload,
            } = record
            else {
                return Err(internal(
                    "conditioned pty write produced a non-suppression record",
                ));
            };
            self.done(Ask::ConditionedEffectSuppressed {
                method: *method,
                target: target.clone(),
                provenance: *provenance,
                reason: reason.clone(),
                payload: payload.clone(),
            })?;
            // Discarded by the route's suppression arm, exactly as `attach`'s `PtyId::default()`
            // is: a suppressed call returns the typed refusal and never reads a host answer. Zero
            // written bytes is the honest filler — no host call happened.
            return Ok(PtyWriteOutcome::Complete { written_len: 0 });
        }

        let applied = operation
            .applied_effect()
            .ok_or_else(|| internal("conditioned pty write persistence had no outcome"))?;
        let ConditionedTarget::Pty { pty } = applied.target() else {
            return Err(internal("pty.write had a non-pty target"));
        };
        let bytes = applied
            .payload()
            .ok_or_else(|| internal("pty.write had no decoded payload"))?
            .to_vec();
        let payload = applied
            .payload_digest()
            .ok_or_else(|| internal("pty.write had no payload digest"))?
            .clone();
        match self.handle.ask(Ask::PtyWrite {
            pty: *pty,
            bytes,
            payload,
            actor: applied.actor(),
            provenance: applied.provenance(),
        })? {
            Answered::PtyWritten(outcome) => Ok(outcome),
            other => Err(internal(format!(
                "the loop answered {other:?} for pty.write"
            ))),
        }
    }
}

impl ConditionedMemoryControl for LoopControl {
    fn append(
        &self,
        operation: PersistenceOperation<'_, MemoryAppendParams>,
    ) -> Result<Lsn, RpcError> {
        let ask = if let Some(record) = operation.disposition_record() {
            let Record::MemoryAppendQuarantined {
                agent,
                payload,
                provenance,
                reason,
            } = record
            else {
                return Err(internal(
                    "conditioned memory append produced a non-quarantine record",
                ));
            };
            Ask::MemoryAppendQuarantined {
                agent: *agent,
                payload: payload.clone(),
                provenance: *provenance,
                reason: reason.clone(),
            }
        } else {
            let applied = operation
                .applied_effect()
                .ok_or_else(|| internal("conditioned memory persistence had no outcome"))?;
            let ConditionedTarget::Agent { agent } = applied.target() else {
                return Err(internal("memory append had a non-agent target"));
            };
            let payload = applied
                .payload()
                .ok_or_else(|| internal("memory append had no decoded payload"))?
                .to_vec();
            Ask::MemoryAppend {
                agent: *agent,
                payload,
                provenance: applied.provenance(),
            }
        };

        match self.handle.ask(ask)? {
            Answered::MemoryAppended(lsn) => Ok(lsn),
            other => Err(internal(format!(
                "the loop answered {other:?} for memory.append"
            ))),
        }
    }
}

impl dudes_rpc::methods::MemoryRead for LoopControl {
    fn read(
        &self,
        agent: AgentId,
        caller: Option<AgentId>,
        since_lsn: Option<Lsn>,
    ) -> Result<MemoryReadResult, RpcError> {
        match self.handle.ask(Ask::MemoryRead {
            agent,
            caller,
            since_lsn,
        })? {
            Answered::MemoryRead(result) => Ok(result),
            other => Err(internal(format!(
                "the loop answered {other:?} for memory.read"
            ))),
        }
    }
}

impl dudes_rpc::methods::MemorySearch for LoopControl {
    fn search(
        &self,
        agent: AgentId,
        caller: Option<AgentId>,
        options: MemorySearchOptions,
    ) -> Result<MemorySearchResult, RpcError> {
        match self.handle.ask(Ask::MemorySearch {
            agent,
            caller,
            options,
        })? {
            Answered::MemorySearch(result) => Ok(result),
            other => Err(internal(format!(
                "the loop answered {other:?} for memory.search"
            ))),
        }
    }
}

impl dudes_rpc::methods::TeamRoster for LoopControl {
    fn roster(
        &self,
        caller: Option<AgentId>,
    ) -> Result<Vec<dudes_proto::rpc::team::RosterEntry>, RpcError> {
        match self.handle.ask(Ask::TeamRoster { caller })? {
            Answered::Roster(rows) => Ok(rows),
            other => Err(internal(format!(
                "the loop answered {other:?} for team.roster"
            ))),
        }
    }
}

/// `JournalRead`, over the journal's own files.
#[derive(Clone, Debug)]
pub struct JournalReader {
    dir: PathBuf,
}

impl JournalReader {
    /// A reader over the journal directory inside the state directory.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl JournalRead for JournalReader {
    fn entries_from(&self, from_lsn: Option<Lsn>) -> Result<Vec<(Lsn, Entry)>, RpcError> {
        // `None` means "nothing yet": a subscriber that asked for the tail wants what comes next,
        // not what already happened, and `Subscribers::appended` carries everything after.
        let Some(from) = from_lsn else {
            return Ok(Vec::new());
        };
        let reader = Reader::new(&self.dir);
        let iter = reader
            .iter_from(from)
            .map_err(|error| internal(format!("the journal could not be read: {error}")))?;
        iter.collect::<Result<Vec<_>, _>>()
            .map_err(|error| internal(format!("the journal could not be read: {error}")))
    }

    fn last_decision(&self, agent: AgentId) -> Result<WhyResult, RpcError> {
        let reader = Reader::new(&self.dir);
        let iter = reader
            .iter_from(Lsn::ZERO)
            .map_err(|error| internal(format!("the journal could not be read: {error}")))?;
        let mut found: Option<WhyResult> = None;
        for item in iter {
            let (lsn, entry) =
                item.map_err(|error| internal(format!("the journal could not be read: {error}")))?;
            if let Record::LoopDecided {
                agent: decided,
                input_digest,
                decision,
                gate_trace,
            } = entry.record
                && decided == agent
            {
                found = Some(WhyResult {
                    lsn,
                    input_digest,
                    decision,
                    gate_trace,
                });
            }
        }
        found.ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                "that agent has not been decided about yet",
            )
        })
    }
}

pub(crate) fn journal_stopped_error(lsn: Lsn) -> RpcError {
    let stopped = dudes_proto::rpc::JournalStoppedData { lsn };
    RpcError {
        code: dudes_proto::rpc::JOURNAL_STOPPED_CODE,
        message: stopped.to_string(),
        data: Some(serde_json::to_value(stopped).expect("lsn serializes")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dudes_proto::{AgentBudget, rpc::ErrorCode};
    use dudes_types::{Provider, Usd};

    use super::*;

    struct Release(Sender<()>);

    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn a_dropped_git_reply_is_unavailable_instead_of_an_unknown_agent() {
        let (jobs, loop_side) = mpsc::channel();
        let handle = Handle::new(jobs);
        std::thread::scope(|scope| {
            let caller = scope.spawn(|| handle.git_target(AgentId::from_parts(1, 2)));
            let Job::GitTarget { reply, .. } = loop_side
                .recv_timeout(Duration::from_secs(30))
                .expect("Git lookup queued")
            else {
                panic!("the job is a Git lookup");
            };
            drop(reply);
            assert!(matches!(caller.join().unwrap(), Err(Unavailable)));
        });
    }

    #[test]
    fn floor_sealing_cannot_pass_an_admitted_sender_before_enqueue() {
        let (jobs, loop_side) = mpsc::channel();
        let handle = Handle::new(jobs);
        let (entered, arrived) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        *handle.floor_submission_pause.lock().unwrap() = Some(FloorSubmissionPause {
            entered,
            release: gate,
        });
        let request = || {
            crate::floor_memory::MemoryRequest::parse(
                crate::floor_memory::MemoryRoute::Page,
                br#"{"agent_id":"01J00000000000000000000000","before":null}"#,
            )
            .expect("page request")
        };
        std::thread::scope(|scope| {
            let release = Release(release);
            let (reply, answers) = mpsc::channel();
            let caller = scope.spawn(|| {
                handle.send_floor(Job::FloorMemory {
                    request: request(),
                    reply,
                })
            });
            arrived
                .recv_timeout(Duration::from_secs(30))
                .expect("submission passed the open check");
            assert!(
                matches!(
                    handle.floor_admission.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "the admitted enqueue still excludes sealing"
            );
            let sealing = scope.spawn(|| handle.stop_waiting());
            drop(release);
            sealing.join().expect("admission sealed");
            let Job::FloorMemory { reply, .. } =
                loop_side.try_recv().expect("accepted before seal")
            else {
                panic!("the accepted Floor memory read was queued");
            };
            drop(reply);
            assert!(caller.join().expect("caller ended").is_ok());
            assert!(answers.recv().is_err());
            assert!(handle.floor_memory(request()).is_none());
            assert!(matches!(
                loop_side.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            let answers = handle
                .ask_queued(Ask::AgentList)
                .expect("ordinary internal ask");
            assert!(matches!(loop_side.try_recv(), Ok(Job::Ask { .. })));
            assert!(answers.recv().is_err());
        });
    }

    fn hire_params() -> HireParams {
        HireParams {
            character: None,
            accent: None,
            name: "Kim".to_owned(),
            role: "operator-authority test".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: PathBuf::from("/work/repo"),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(1)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            checkout: None,
            from: None,
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn hire_seals_before_delivery_and_a_failure_never_reopens() {
        let source = Arc::new(dudes_rpc::OperatorAuthoritySource::from_replayed_agents(
            false,
        ));
        let (jobs, loop_side) = mpsc::channel();
        let control = LoopControl::new(Handle::new(jobs), Arc::clone(&source));

        std::thread::scope(|scope| {
            let calling = scope.spawn(|| control.hire(hire_params()));
            let Job::Ask { ask, reply } = loop_side.recv().expect("the hire reached the loop")
            else {
                panic!("the delivered job is the hire")
            };
            assert!(matches!(ask, Ask::Hire(_)));
            assert!(
                source.is_sealed(),
                "the epoch is sealed before Ask::Hire becomes observable to the loop"
            );
            reply
                .send(Err(error(
                    ErrorCode::InternalError,
                    "injected provider failure after delivery",
                )))
                .expect("the caller still waits for the answer");
            assert!(
                calling.join().expect("the caller did not panic").is_err(),
                "the injected post-seal failure reaches the caller"
            );
        });

        assert!(
            source.is_sealed(),
            "a validation, provider, persistence, or spawn failure cannot reopen acquisition"
        );
    }
}
