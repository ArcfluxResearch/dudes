//! What reaches the machine.
//!
//! Everything here is a *value the runtime already has* — a parsed command, an acknowledged
//! append, an observation from the process host. Nothing in this module is JSON, and nothing in
//! it is read from the world: the runtime parses and observes, the machine decides.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dudes_policy::WorkInput;
use dudes_proto::Spend;
use dudes_proto::goal::{Evidence, Goal, Predicate};
use dudes_proto::record::{
    BrowserAnswer, BrowserControl, BrowserGoal, BrowserRename, BrowserRetire, Hex32,
};
use dudes_proto::rpc::pty::PtyId;
use dudes_proto::{
    Actor, AgentBudget, ConditionedMethod, ConditionedTarget, Entry, Event, ExitStatus, GoalBudget,
    LiveActivityKind, PayloadDigest, PeerProvenance, TurnOutcome,
};
use dudes_providers::launch::LaunchSpec;
use dudes_providers::stream::{ScopedUsage, SessionObservation};
use dudes_types::{
    AgentId, BreakerSignal, GoalId, IsolationMode, Lsn, Provider, SessionId, TurnId, Usage, Usd,
};

use crate::host::{AdoptionKey, Liveness};

/// The persist-before-spawn ledger's correlation id.
///
/// **Not the agent token.** This is the *persist token*, named that precisely to keep the two
/// apart: this one correlates an [`Effect::Persist`](crate::Effect::Persist) with the
/// [`Input::Persisted`] that acknowledges its fsync, and it never leaves the daemon. The agent
/// token is a secret handed to a child in `LaunchSpec.env`, and it never enters this vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersistToken(pub u64);

/// One volatile mail admission refresh, never journal or launch authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailWakeRequest(pub u64);

/// Volatile correlation for one bounded instruction sample, never durable authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstructionRequest(pub u64);

/// A refresh could not provide current admission facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailWakeRefreshFailure {
    /// Current day-specific budget projections were unavailable.
    SpendUnavailable,
    /// Canonical progression requires journal/mailbox recovery.
    JournalUnresolved,
}

/// One hook call in flight, so its [`Reply`](crate::Effect::Reply) reaches the right waiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HookId(pub u64);

/// One subscribed RPC connection, so a [`Notify`](crate::Effect::Notify) reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u64);

/// A breaker signal the runtime observed, in the four shapes a v1 emitter can produce.
///
/// **Deliberately not [`BreakerSignal`] itself.** That enum also carries `InferredTurnEnd` and
/// `IsolationLeak`, neither of which crosses this runtime-input seam. The rule is that *no
/// emitter writes an `Inferred` turn end* — a
/// managed end is `ChildExited` or the reaper's `Observed` `Vanished`, attached-mode quiescence
/// is [`Self::PtySilence`] and a hook timeout is [`Self::HookTimeout`]. That is what makes gate
/// 2's `Hold` transient by construction.
///
/// A seam typed as `BreakerSignal` would let a runtime hand `InferredTurnEnd` in and leave the
/// rule as a comment. Typed this way it is a compile error.
///
/// **What this narrows is the runtime, and only the runtime.** It is not a claim that either
/// excluded signal is unreachable everywhere. Gate 2 stays as
/// `last_turn_end == Inferred → Hold{InferredTurnEnd}` accompanied by a breaker signal raised
/// by the supervisor on the next beat — and a supervisor-raised signal goes straight into
/// `Supervisor::signals`, a `BTreeSet<BreakerSignal>`, without crossing this seam. That path is
/// already open and needs no variant here; the isolation observation can use that same path.
/// `decide()` likewise still knows the full journal vocabulary,
/// because gate 2 must not know which emitters exist (invariant 5). A variant is added to *this*
/// enum only when a **runtime** observes something none of the four shapes covers — that is the
/// change that amends this set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RuntimeSignal {
    /// The supervisor did not answer a hook within its deadline and the hook server failed open.
    HookTimeout,
    /// An attached session's PTY stayed quiet.
    ///
    /// **Nothing produces this yet, and the cadence below is what a rule assumes, not what a
    /// runtime does.** The compaction suppression ceiling is denominated in beats, so it
    /// bounds wall time only if the signal is *level-triggered* — present in the collected set on
    /// every beat the agent stays quiet. An edge-triggered producer, emitting once per silence
    /// episode, would advance the count at most once per episode and the ceiling would bound
    /// nothing. The change that lands the producer must state the cadence and add the test that pins it;
    /// until then, this paragraph is the assumption's only
    /// record and no test can tell the two producers apart.
    PtySilence,
    /// A standing steer hit the vendor's consecutive-block cap and blocking stopped at 7.
    StopCap,
    /// Five notifications for one mail item went unacknowledged.
    UnackedMail,
}

impl RuntimeSignal {
    /// Every variant, so a test walks them without keeping a second list in step.
    ///
    /// **Adding a variant? Add it here too — nothing will tell you.** This array is hand-written
    /// and **unguarded**, and you are standing in the one file where that matters: the compile
    /// error that brought you here is [`Self::signal`]'s match, forty lines below, and **no error
    /// ever points at this array**. A `[Self; 4]` left at four still compiles, and every test that
    /// walks it then covers four of five and stays green. `BreakerSignal::ALL`, which this is the
    /// runtime subset of, does not have that problem — `dudes-types`'
    /// `breaker_signal_all_is_every_variant` re-derives it from serde's derive — but this enum has
    /// no serde derive to re-derive from, and being `#[non_exhaustive]` it cannot be matched
    /// exhaustively from another crate either.
    ///
    /// Partly, not wholly, mitigated: `reachable_signals()` in `tests/breaker.rs` asserts this
    /// array's image under [`Self::signal`] equals `BreakerSignal::ALL` minus `InferredTurnEnd`
    /// and `IsolationLeak`,
    /// which reddens for a variant omitted here whose journaled signal is new, or added here and
    /// mapped onto an existing one. It stays **green** for a variant omitted here and mapped onto
    /// an existing journaled signal — a second runtime source for one signal — and that helper's
    /// doc carries the measured table.
    ///
    /// Closing it needs no dependency and no derive macro: a `macro_rules!` declaring the variants
    /// once and expanding to both the enum and this array does it, `#[non_exhaustive]` and doc
    /// comments intact.
    pub const ALL: [Self; 4] = [
        Self::HookTimeout,
        Self::PtySilence,
        Self::StopCap,
        Self::UnackedMail,
    ];

    /// The journaled signal this one is. Total, and no arm yields
    /// [`BreakerSignal::InferredTurnEnd`].
    ///
    /// A new arm here means a new entry in [`Self::ALL`], which the compiler will not ask for.
    #[must_use]
    pub const fn signal(self) -> BreakerSignal {
        match self {
            Self::HookTimeout => BreakerSignal::HookTimeout,
            Self::PtySilence => BreakerSignal::PtySilence,
            Self::StopCap => BreakerSignal::StopCap,
            Self::UnackedMail => BreakerSignal::UnackedMail,
        }
    }
}

/// Spend in the three scopes gate 5 admits against, as the caller computed it.
///
/// **The supervisor accumulates no cost.** `World` folds lifecycle state; the money arrives here,
/// from the redb budget projections the runtime owns. One rule — `TurnEnded.usage`
/// bucketed by the UTC day of `Entry.at` — with one implementation, on the read path, rather than
/// a second copy inside a machine that cannot read the journal anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpendView {
    /// Against the agent's current goal.
    pub goal: Spend,
    /// Unobserved ends against the current goal.
    pub goal_unknown_ends: u32,
    /// Against the agent's daily ceiling, already rolled over.
    pub agent_today: Spend,
    /// The agent's unobserved ends today.
    pub agent_unknown_ends_today: u32,
    /// Across every agent today.
    pub floor_today: Spend,
    /// The floor's unobserved ends today.
    pub floor_unknown_ends_today: u32,
}

/// Everything the machine reacts to.
#[derive(Debug)]
#[non_exhaustive]
pub enum Input {
    /// An external mailbox append was acknowledged in canonical LSN order.
    Journaled {
        /// Canonical position, without a supervisor persist token.
        lsn: Lsn,
        /// The acknowledged mailbox entry.
        entry: Entry,
    },
    /// A runtime-owned instruction or PTY boundary append is durable and projected in LSN order.
    InstructionJournaled {
        /// Exact caller frame position, without a supervisor persist token.
        lsn: Lsn,
        /// The acknowledged instruction or existing PTY close boundary entry.
        entry: Entry,
    },
    /// Current facts for exactly one still-owned mail refresh.
    MailWakeReady {
        /// Recipient.
        agent: AgentId,
        /// Volatile request identity.
        request: MailWakeRequest,
        /// One clock sample, its spend and a volatile projection cursor, or failure.
        result: Result<(dudes_types::Timestamp, SpendView, Lsn), MailWakeRefreshFailure>,
    },
    /// The agents whose recorded worktree the daemon found gone or no longer a worktree,
    /// computed from the file system before [`Input::Started`]. It appends nothing
    /// and only remembers the set: `Started`'s first step holds each of these agents, so the first
    /// decide cannot issue or re-issue a turn into a missing tree. The daemon never falls back to
    /// the repository.
    MissingWorktrees(Vec<AgentId>),
    /// Replay is finished; run the three crash rules and decide. Sent exactly once per daemon
    /// run, after the last [`Input::Replayed`] and **before the first append** — `Reader::locate`
    /// catches a re-created current segment only while no append has happened since.
    Started,
    /// Reconsider only the same current work after its durable instruction review acknowledgment.
    InstructionReviewAcknowledged {
        /// Current goal owner.
        agent: AgentId,
        /// The same ticket goal; a changed or cleared goal is not resumed.
        goal: GoalId,
    },
    /// Current canonical card eligibility for an exact pending recovery lookup.
    InstructionRecoveryTicketResolved {
        /// Original volatile lookup identity.
        request: InstructionRequest,
        /// Original historical association if still eligible, otherwise none.
        ticket: Option<dudes_proto::InstructionTicketAssociation>,
    },
    /// The actual host retained this physical attempt after an unconfirmed launch cleanup.
    InstructionLaunchCleanupUnconfirmed {
        /// Immutable issued turn whose current starting boundary owned the host call.
        turn: TurnId,
    },
    /// Bounded filesystem evidence for the exact requested observation boundary.
    InstructionSampled {
        /// Exact volatile request.
        request: InstructionRequest,
        /// Actual sampled endpoint, including named uncertainty.
        snapshot: Box<dudes_proto::InstructionSnapshot>,
        /// Current ticket interval captured by the writer at the starting request.
        ticket: Option<dudes_proto::InstructionTicketAssociation>,
        /// Ending comparison against the supplied durable baseline; absent for starting samples.
        comparison: Option<Box<dudes_proto::InstructionComparison>>,
    },
    /// One entry from the journal, in `lsn` order. Feeds the *same* fold as [`Input::Persisted`],
    /// which is what makes a replayed decision equal to the live one (invariant 1).
    Replayed {
        /// Where the frame that carried this entry sits in the log.
        ///
        /// Carried here because [`Entry`] does not carry it: the frame header holds the position
        /// and the payload does not, so the reader is the only thing that knows it and the fold
        /// can only learn it from its caller. [`Input::Persisted`] already carries the same value
        /// for the live path, so both folds are fed one position per entry and a replayed fold
        /// sees what the live fold saw.
        lsn: Lsn,
        /// What the frame carried.
        entry: Entry,
    },
    /// A typed, already-parsed RPC call.
    Command(Command),
    /// A normalised hook event, with the id its reply must carry.
    Hook {
        /// The call in flight.
        hook: HookId,
        /// What the provider said.
        event: Event,
    },
    /// An [`Effect::Persist`](crate::Effect::Persist) reached the disk. The entry is folded now,
    /// never at emit time: a fold at emit time would leave the in-memory world ahead of the
    /// journal after a crash, and replay would then decide differently.
    ///
    /// **Ordering is a contract the caller owes.** One `handle()` can emit several `Persist`
    /// effects; they must be appended **and acknowledged in emission order**. The live path folds
    /// in acknowledgement order and replay folds in `lsn` order, so an out-of-order
    /// acknowledgement makes those two sequences differ — and "replay decides what the live run
    /// decided" quietly stops being true, without any test failing. Same reason
    /// [`Input::Started`] must follow the last [`Input::Replayed`] and precede the first append.
    Persisted {
        /// Which persist.
        token: PersistToken,
        /// Where it landed.
        lsn: Lsn,
    },
    /// The host launched a child and read its adoption key. An observation after the fact;
    /// nothing waits on it.
    Spawned {
        /// The turn.
        turn: TurnId,
        /// How the child is recognised after a crash.
        key: AdoptionKey,
        /// The daemon-selected permission carried through the actual host call.
        mcp_grant: Option<Lsn>,
    },
    /// The host positively established that its launch attempt left no process running.
    SpawnFailed {
        /// The turn whose launch failed.
        turn: TurnId,
        /// The host error that prevented launch, rendered by the runtime.
        reason: String,
    },
    /// Fallible preparation failed before any process-host call for this exact attempt.
    LaunchPreparationFailed {
        /// Immutable issued work.
        turn: TurnId,
        /// Captured physical starting observation.
        started_lsn: Lsn,
        /// Bounded runtime failure copy.
        reason: String,
    },
    /// Admission refused before the host was called; an issued task remains owed.
    LaunchAdmissionRefused {
        /// Owning agent.
        agent: AgentId,
        /// Issued managed work, or an owned Attached launch without a managed issue.
        turn: Option<TurnId>,
        /// Typed current evidence, without provider launch inputs.
        refusal: crate::SharedFolderRefusal,
        /// The not-appended browser dispatch intent being refused, when applicable.
        dispatch_token: Option<PersistToken>,
    },
    /// The exact Attached port call failed after its starting observation was durable.
    InstructionAttachedOpenFailed {
        /// Captured physical starting observation, unaffected by a successor attachment.
        started_lsn: Lsn,
        /// The host retained cleanup whose absence was not confirmed.
        cleanup_unconfirmed: bool,
    },
    /// Earlier durable acknowledgments have folded; retry the same unappended browser intent.
    BrowserDispatchAdmissionReady {
        /// Exact parked intent.
        token: PersistToken,
        /// Exact issued work.
        turn: TurnId,
    },
    /// The stream reader saw the vendor's session id for this turn.
    SessionLearned {
        /// The turn.
        turn: TurnId,
        /// The id.
        session: SessionId,
    },
    /// A structured managed program event, correlated by the host to one turn.
    ActivityObserved {
        /// The turn whose stream contained the event.
        turn: TurnId,
        /// Fixed kind only. No program payload crosses this boundary.
        kind: LiveActivityKind,
    },
    /// Admit at most one pending fixed-kind sample per agent on a one-second timer.
    ActivityFlush,
    /// A managed child exited. Carries the adapter's `outcome`, because a supervisor cannot tell
    /// `MaxTurns` from `BudgetExhausted` from an exit code — only the vendor's `result` line can,
    /// and inventing one here would put an `Inferred` verdict where invariant 3 forbids it.
    ChildExited {
        /// The turn.
        turn: TurnId,
        /// Monotonic runtime duration, when measured by the daemon.
        elapsed_ms: Option<u64>,
        /// How the process ended.
        exit: ExitStatus,
        /// What the stream said it consumed, and over what span. The supervisor turns it into the
        /// turn's charge, the only thing budget admission charges.
        usage: Option<ScopedUsage>,
        /// What the provider attributed to each model it named, keys as reported.
        /// Evidence carried to `TurnEnded` and read by no gate: empty is "not reported", and a
        /// partial cost that `usage` refused still arrives here in full.
        model_usage: BTreeMap<String, Usage>,
        /// How the turn ended, as the adapter read it.
        outcome: TurnOutcome,
        /// The model's final message, when the stream carried one.
        last_message: Option<String>,
        /// Provider-attributed final reply with explicit completeness.
        final_reply: Option<dudes_proto::ChatReply>,
    },
    /// The reaper's answer to an [`Effect::Probe`](crate::Effect::Probe). The runtime executes
    /// the probe; **the supervisor alone decides what it means**.
    ///
    /// `Gone` writes `TurnEnded{Observed, exit: UNKNOWN, usage: None}` and the predicate runs as
    /// usual, so `Done` is reachable after a crash; `Unknown` writes **nothing** and the turn
    /// stays open, and a standing kill against it is journaled as deferred rather than signalled;
    /// `Alive` adopts.
    ///
    /// **A refused kill arrives here too**, not as a new record: `ProcessHost::kill` answering
    /// [`KillOutcome::Refused`](crate::KillOutcome::Refused) is the reaper's finding by a second
    /// route, and this variant's rules already say what each answer means.
    Liveness {
        /// The turn probed.
        turn: TurnId,
        /// What the host could say.
        liveness: Liveness,
    },
    /// A predicate run finished. `Observed` by construction: the daemon ran it.
    PredicateDone {
        /// The goal judged.
        goal: GoalId,
        /// Whether it held.
        pass: bool,
        /// What it rested on.
        evidence: Evidence,
    },
    /// An attached PTY exited; the keyboard comes back to the loop.
    PtyExited {
        /// The agent.
        agent: AgentId,
    },
    /// Fresh spending for admission.
    Spend {
        /// Whose.
        agent: AgentId,
        /// The three numbers.
        view: SpendView,
    },
    /// A breaker signal the runtime observed for one agent.
    ///
    /// **Nothing moves when this arrives.** The signal is collected and the rung is moved by the
    /// next [`Input::Tick`], which is the beat: the ladder climbs at most one rung per beat,
    /// so a burst inside one window is one rung and one `BreakerBeat`, not one each.
    /// A signal collected for an agent with no ladder — retired, or never hired — reaches no
    /// beat and is dropped by the next one.
    Signal {
        /// Whose breaker.
        agent: AgentId,
        /// What the runtime saw.
        signal: RuntimeSignal,
    },
    /// The beat. Carries no `Instant`: [`Clock`](crate::Clock) is the single source of time in
    /// this crate, and a second one would not be reproducible under `FakeClock`.
    ///
    /// One beat per non-retired agent, each calling [`dudes_policy::beat()`] over the signals
    /// collected since that agent's last beat.
    Tick,
    /// The daemon's current work carrier, refreshed without deciding or emitting effects.
    /// Projection reads and mailbox observations remain outside this sans-io machine.
    Work {
        /// Whose.
        agent: AgentId,
        /// The three values.
        view: WorkInput,
    },
    /// What a managed turn's stream reported as its session opened.
    ///
    /// The host supplies the physical key and launch permission; the vendor selects neither.
    /// Agent and adapter still come from the journal fold.
    SessionObserved {
        /// The turn whose child reported it.
        turn: TurnId,
        /// The exact physical child whose reader observed the line.
        key: AdoptionKey,
        /// The permission fixed before that physical launch.
        mcp_grant: Option<Lsn>,
        /// What it reported.
        observation: SessionObservation,
    },
    /// The PTY host started an attached agent's child and the process host registered its key.
    /// An observation after the fact, like [`Input::Spawned`].
    PtySpawned {
        /// The attached agent.
        agent: AgentId,
        /// The run-local PTY handle.
        pty: PtyId,
        /// How the child is recognised, in this run and after a restart.
        key: AdoptionKey,
    },
    /// A board record the runtime has appended and projected durably. Fold it, invalidate the
    /// changed card's cached evidence, and judge goals holding that card before another turn.
    BoardChanged {
        /// Where the durable record landed.
        lsn: Lsn,
        /// A `CardCreated`, `Claim`, or `CardTransition` entry. Other records are ignored.
        entry: Entry,
    },
    /// A bounded resolver worker returned a revocable browser launch preparation.
    BrowserLaunchReady {
        /// The immutable issued turn.
        turn: TurnId,
        /// The unique run-local preparation generation.
        generation: u64,
        /// A resolved absolute launch or a confirmed preparation refusal.
        prepared: Result<Box<LaunchSpec>, String>,
    },
    /// A predicate run ended without an observed verdict.
    PredicateEndedWithoutVerdict {
        /// The goal whose run ended.
        goal: GoalId,
    },
}

/// An RPC call, parsed.
///
/// The runtime owns the method table, so it parses `params` and answers `-32602` itself; a
/// machine whose only reply effect is hook-shaped has no way to answer a malformed request, and
/// giving it one would put `serde_json` inside `handle()`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    /// One finite browser hire with a pre-minted intended goal in its durable record.
    BrowserOnboardHire(Box<BrowserOnboardHire>),
    /// Append only the recorded initial goal for exactly its hired agent.
    BrowserOnboardGoal(Box<BrowserOnboardGoal>),
    /// `agent.hire`.
    Hire {
        /// Display name.
        name: String,
        /// Authored cast character. Absence preserves the automatic appearance.
        character: Option<dudes_types::CastCharacter>,
        /// Named accent colour. Absence preserves the neutral accent.
        accent: Option<dudes_types::AccentColour>,
        /// Role text.
        role: String,
        /// Department.
        department: String,
        /// Which CLI drives it.
        provider: Provider,
        /// The repository it works in.
        cwd: PathBuf,
        /// Enrolled workspace metadata admitted by the daemon, when uniquely matched.
        workspace: Option<dudes_proto::record::RecordedWorkspace>,
        /// Daily ceiling.
        budget: AgentBudget,
        /// The provider's sandbox on-switch, as the hire asked for it.
        sandbox: Option<bool>,
        /// The managed-turn isolation selection, as the hire asked for it.
        isolation: Option<IsolationMode>,
        /// The vendor-run turn cap, as the hire asked for it.
        max_turns: Option<u32>,
        /// The vendor-run budget backstop, as the hire asked for it.
        max_budget: Option<Usd>,
        /// The operator's trust declaration for `cwd`; `false` when none was made.
        trusted: bool,
        /// The lease `cwd` is the tree of, when the hire is into a worktree. The daemon
        /// has appended its `WorktreeLeased` first.
        worktree: Option<dudes_types::WorktreeId>,
        /// The declared MCP servers the daemon admitted for this hire, sorted and without repeats.
        mcp_servers: Vec<dudes_types::McpServerName>,
    },
    /// Lease a worktree for an agent that is not hired yet. Appended and fsynced
    /// before `git worktree add` runs.
    ///
    /// The machine draws the lease's identity and derives the tree (`<data_dir>/worktrees/<id>`)
    /// and the branch (`dudes/<first 12 hexadecimal characters>`) from it, so neither can disagree
    /// with the identity. The caller finds the new lease as the key the world's table gained.
    LeaseWorktree {
        /// The repository the tree is added from.
        origin: PathBuf,
        /// The commit the branch is created at.
        base: dudes_types::GitCommit,
        /// The lease this one is based on.
        from: Option<dudes_types::WorktreeId>,
    },
    /// End a lease whose tree is gone. Ignored unless the lease's state allows it:
    /// `NeverAssigned` only while pending, `Operator` only once released.
    RemoveWorktree {
        /// The lease.
        worktree: dudes_types::WorktreeId,
        /// Who removed the tree.
        how: dudes_proto::WorktreeRemoval,
    },
    /// `agent.retire`, or the Floor's retirement of one agent.
    Retire {
        /// The agent.
        agent: AgentId,
        /// Why.
        reason: String,
        /// The Floor's attribution, already checked on the writer against the agent's liveness;
        /// `None` for the local operator.
        browser_retire: Option<BrowserRetire>,
    },
    /// `goal.set`, or the Floor's replacement of one agent's current goal.
    GoalSet {
        /// Who pursues it.
        agent: AgentId,
        /// The goal as the model reads it.
        text: String,
        /// How the daemon decides it is met.
        predicate: Predicate,
        /// Ceiling for the whole goal.
        budget: GoalBudget,
        /// Most managed turns it may consume.
        hop_cap: u32,
        /// The Floor's attribution, already checked on the writer against the agent's current
        /// goal and carrying what that goal had left; `None` for the local
        /// operator, whose command is unconditional.
        browser_goal: Option<Box<BrowserGoal>>,
        /// Local replacement attribution, when admitted on the writer.
        goal_replacement: Option<Box<dudes_proto::record::GoalReplacementAttribution>>,
        /// The check's recorded name, when it has one.
        completion_check: Option<dudes_proto::CompletionCheckRef>,
        /// What goal set saw of the check before the first turn.
        baseline: Option<dudes_proto::CheckBaseline>,
    },
    /// The start of a ticket: one record that installs the ticket's goal and moves its card to
    /// Working. The daemon chooses the baseline, `MayAlreadyPass`, and never runs the check first.
    TicketStart {
        /// The ticket.
        card: dudes_types::CardId,
        /// The agent that holds it in its queue.
        agent: AgentId,
        /// The goal as the model reads it.
        text: String,
        /// The ticket's check.
        predicate: Predicate,
        /// Ceiling for the whole goal.
        budget: GoalBudget,
        /// Most managed turns it may consume.
        hop_cap: u32,
        /// The check's recorded name.
        completion_check: Option<dudes_proto::CompletionCheckRef>,
        /// The card's sequence the daemon decided against.
        expected_seq: dudes_types::Seq,
    },
    /// `goal.clear`.
    GoalClear {
        /// The agent.
        agent: AgentId,
        /// The goal.
        goal: GoalId,
    },
    /// `loop.halt`.
    Halt {
        /// The agent.
        agent: AgentId,
        /// Why, when given.
        text: Option<String>,
    },
    /// `loop.pause`, or the Floor's pause of one agent.
    Pause {
        /// The agent.
        agent: AgentId,
        /// The Floor's attribution, already checked on the writer against the agent's control
        /// identity; `None` for the local operator, whose command is unconditional.
        browser: Option<BrowserControl>,
    },
    /// `loop.resume`, or the Floor's resume of one agent.
    Resume {
        /// The agent.
        agent: AgentId,
        /// The Floor's attribution, as on [`Command::Pause`].
        browser: Option<BrowserControl>,
    },
    /// `agent.steer`.
    Steer {
        /// The agent.
        agent: AgentId,
        /// Context to inject on the next event.
        text: String,
        /// The accepted connection's exact provenance, or `None` for a legacy/internal producer.
        /// Invariant 8 forbids reconstructing this from params.
        provenance: Option<PeerProvenance>,
    },
    /// `agent.answer`; clears `Waiting`.
    Answer {
        /// The agent.
        agent: AgentId,
        /// The answer.
        text: String,
        /// The accepted connection's exact provenance, or `None` for a legacy/internal producer.
        /// Invariant 8 forbids reconstructing this from params.
        provenance: Option<PeerProvenance>,
    },
    /// A conditioned effect withheld by the central disposition table.
    ///
    /// This is typed rather than a generic record command so the runtime cannot turn its
    /// persistence port into a second arbitrary journal writer (invariant 1).
    ConditionedEffectSuppressed {
        /// Which conditioned method was withheld.
        method: ConditionedMethod,
        /// The target derived from the method's typed params.
        target: ConditionedTarget,
        /// The accepted connection's exact provenance (invariant 8).
        provenance: PeerProvenance,
        /// The table's stable reason.
        reason: String,
        /// Digest of the decoded payload, when the method carries one.
        payload: Option<PayloadDigest>,
    },
    /// `agent.attach`: a human takes the keyboard.
    Attach {
        /// The agent.
        agent: AgentId,
        /// The operator peer or the browser terminal's session, resolved by the runtime — never a
        /// field the caller filled in (invariant 8).
        by: Actor,
        /// The accepted connection's exact provenance (invariant 8), absent for a browser
        /// terminal.
        provenance: Option<PeerProvenance>,
        /// Requested terminal columns.
        cols: u16,
        /// Requested terminal rows.
        rows: u16,
    },
    /// Confirmed PTY close after `agent.detach`; the caller already journaled the host outcome.
    Detach {
        /// The agent.
        agent: AgentId,
        /// Who gave the keyboard back.
        by: Actor,
        /// The explicit peer's exact provenance, or `None` for a system retry.
        provenance: Option<PeerProvenance>,
    },
    /// `floor.subscribe`: send this connection notifications until it unsubscribes.
    Subscribe {
        /// The connection.
        client: ClientId,
    },
    /// `floor.unsubscribe`.
    Unsubscribe {
        /// The connection.
        client: ClientId,
    },
    /// `agent.ask`: the agent asks its human something, and gate 3 holds it until the answer.
    ///
    /// The **only** way a `Record::HumanQuestion` is written. It is a
    /// command and not an `Event` on purpose: `Event` is the vendor's stream, and a
    /// Dudes-originated fact riding a vendor-shaped type is how the rule that no classification of an
    /// agent's output may raise gate 3 comes back in through a side door.
    //
    // Appended rather than filed beside `Answer`, which reads better and costs a major break:
    // `cargo semver-checks` shows that inserting it there moves five
    // discriminants and reported `enum_no_repr_variant_discriminant_changed` naming `Answer`,
    // `Attach`, `Detach`, `Subscribe` and `Unsubscribe`. `#[non_exhaustive]` does not cover that —
    // it stops a downstream `match` from breaking, not a downstream `as isize`. Appended, this
    // crate reports **no findings at all**: `enum_variant_added` is defined for enums *without*
    // `#[non_exhaustive]`, so the attribute pays for the variant and the placement pays for the
    // discriminants. Adding one here is free; adding one in the middle is not.
    Ask {
        /// The agent, as the seam resolved it from the token. Never a field the caller filled in
        /// (invariant 8).
        agent: AgentId,
        /// The question.
        text: String,
    },
    /// The Floor's opt-in answer to one open question, appended with
    /// `provenance: None` beside its browser attribution. Appended after `Ask` for the reason
    /// given there.
    BrowserAnswer(Box<BrowserAnswerCommand>),
    /// Rename display text without changing structural identity or loop state.
    Rename {
        /// The live agent.
        agent: AgentId,
        /// The admitted new name.
        name: String,
        /// Attribution of a browser rename, absent for a local operator.
        browser_rename: Option<BrowserRename>,
    },
    /// Change a live agent's role and appearance without changing its engine or model.
    Look {
        /// The live agent.
        agent: AgentId,
        /// Admitted display role.
        role: String,
        /// Authored character, or automatic selection.
        character: Option<dudes_types::CastCharacter>,
        /// Named accent, or automatic selection.
        accent: Option<dudes_types::AccentColour>,
        /// Attribution of a browser edit, absent for a local operator.
        browser_look: Option<dudes_proto::record::BrowserLook>,
    },
    /// Record an admitted complete desired-server replacement without scheduling work.
    ReplaceMcp {
        /// The unchanged agent identity.
        agent: AgentId,
        /// Complete canonical desired set.
        mcp_servers: Vec<dudes_types::McpServerName>,
    },
    /// Queue a finite message at the current no-goal revision.
    ConversationMessageQueued {
        /// Owning agent.
        agent: AgentId,
        /// Normalized message.
        text: String,
        /// Browser grant and request digests.
        browser: dudes_proto::BrowserNoteAttribution,
        /// Immutable context revision.
        context_lsn: Lsn,
        /// Exact context loss explicitly acknowledged by this new request.
        context_reset_turn: Option<TurnId>,
    },
    /// Queue one goal note or Attached context.
    BrowserNoteQueued {
        /// The destination agent.
        agent: AgentId,
        /// The normalized full note.
        text: String,
        /// Browser request and session digests.
        browser: dudes_proto::BrowserNoteAttribution,
        /// The immutable admitted destination.
        destination: dudes_proto::BrowserNoteDestination,
    },
    /// Cancel an undelivered browser note under the writer's launch claim.
    BrowserNoteCancel {
        /// The owning agent.
        agent: AgentId,
        /// The exact queued note identity.
        note_lsn: Lsn,
        /// Browser request and session digests.
        browser: dudes_proto::BrowserNoteAttribution,
    },
}

/// One admitted Floor answer, already checked on the writer against the open question it names.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserAnswerCommand {
    /// The agent whose open question it closes.
    pub agent: AgentId,
    /// The trimmed, bounded answer text.
    pub text: String,
    /// Request identity, digest, question position and admitting session digest.
    pub attribution: BrowserAnswer,
}

impl std::fmt::Debug for BrowserAnswerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserAnswerCommand(<redacted>)")
    }
}

/// Values admitted by the daemon from an operator-enrolled profile and bounded browser text.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserOnboardHire {
    /// Bounded display name from the authenticated request.
    pub name: String,
    /// Authored cast character. Absence preserves the automatic appearance.
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent colour. Absence preserves the neutral accent.
    pub accent: Option<dudes_types::AccentColour>,
    /// Bounded role text from the authenticated request.
    pub role: String,
    /// Operator-enrolled department.
    pub department: String,
    /// The engine the browser chose among those the daemon offered.
    pub provider: Provider,
    /// Canonical operator-enrolled repository directory.
    pub cwd: PathBuf,
    /// Operator-enrolled agent ceiling.
    pub budget: AgentBudget,
    /// Operator-enrolled isolation selection, for an engine that takes one.
    pub isolation: Option<IsolationMode>,
    /// Bounded initial goal text.
    pub goal_text: String,
    /// Operator-enrolled completion predicate.
    pub predicate: Predicate,
    /// The enrolled check's name and revision, recorded on the intended goal.
    pub completion_check: Option<dudes_proto::CompletionCheckRef>,
    /// What the hire's check run saw before the first turn, recorded on the intended goal.
    pub baseline: Option<dudes_proto::CheckBaseline>,
    /// Operator-enrolled goal ceiling.
    pub goal_budget: GoalBudget,
    /// Operator-enrolled positive turn limit.
    pub hop_cap: u32,
    /// Decoded browser attempt identity.
    pub attempt_id: Hex32,
    /// Digest of the canonical request fields.
    pub request_digest: Hex32,
    /// Opaque enrollment identifier.
    pub profile_id: String,
    /// Operator-enrolled workspace label captured by this hire.
    pub workspace_label: String,
    /// Digest of the admitting Floor session.
    pub session_digest: Hex32,
}

impl std::fmt::Debug for BrowserOnboardHire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserOnboardHire(<redacted>)")
    }
}

/// The exact intended goal read from a durable browser hire.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserOnboardGoal {
    /// Agent named by the durable hire.
    pub agent: AgentId,
    /// Exact goal pre-minted and recorded on the hire.
    pub goal: Goal,
    /// Decoded browser attempt identity.
    pub attempt_id: Hex32,
    /// Digest of the session admitting this later append.
    pub session_digest: Hex32,
}

impl std::fmt::Debug for BrowserOnboardGoal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserOnboardGoal(<redacted>)")
    }
}
